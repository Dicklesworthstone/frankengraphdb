//! Native hybrid CALL uses one seed-directed source, including graph-only
//! and capability-scoped execution. Whole-graph work is a negative control.

use super::*;
use crate::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_beacon::WorkBudget;
use fgdb_delta_types::SchemaEpoch;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x74; 32]);
const TEXT: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100, 10_000_000, 1_000_000)
}

async fn fixture(cx: &CommitCx, disconnected: usize, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x73; 32], NS, [0x75; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for id in [1, 2, 3, 9] {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![
                (TEXT, CanonicalScalar::ucs_basic_text("graph").unwrap()),
                (X, CanonicalScalar::Int(id as i64)),
            ],
        );
    }
    batch.add_edge(EId(1), VId(1), VId(2), vec![]);
    batch.add_edge(EId(2), VId(2), VId(3), vec![]);
    for id in 0..disconnected {
        batch.add_edge(EId(100 + id as u128), VId(9), VId(9), vec![]);
    }
    if hidden {
        batch.create_vertex(
            VId(90),
            vec![LabelId(99)],
            vec![
                (
                    TEXT,
                    CanonicalScalar::ucs_basic_text("graph graph").unwrap(),
                ),
                (X, CanonicalScalar::Int(0)),
            ],
        );
        batch.add_edge(EId(90), VId(1), VId(90), vec![]);
        batch.add_edge(EId(91), VId(90), VId(3), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut shortcut = WriteBatch::new(RelationId(99));
        shortcut.add_edge(EId(92), VId(1), VId(3), vec![]);
        db.write(cx, shortcut).await.unwrap();
    }
    db
}

/// Construct the bound execution value directly for source-work controls;
/// public text/parameter binding and Warden delivery are exercised below.
fn search(vector: bool, text: bool) -> HybridSearch {
    let mut options = Options::text(TEXT);
    options.vertex_label = Some(LabelId(1));
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    let profile = if vector || text {
        ExactRrfProfile::new(60, u16::from(vector), u16::from(text)).unwrap()
    } else {
        GraphHybridQuery::graph_only(1, 1, 100, 60)
            .unwrap()
            .retrieval
            .profile
    };
    HybridSearch {
        options,
        text: "graph".into(),
        text_mode: TextMatch::Any,
        vector: vec![0.0],
        vector_mode: VectorSearch::Exact,
        k: 1,
        candidates: 1,
        profile,
        seeds: vec![VId(1)],
        relation: None,
        direction: ExpansionDirection::Outgoing,
        max_hops: 2,
        include_seeds: false,
        graph_candidates: 1,
        graph_weight: 100,
        outputs: vec![0, 7, 2, 3],
        graph_access: GraphAccess::Indexed,
    }
}

fn execute(
    search: &HybridSearch,
    db: &Database<MemVfs>,
    eligible: Option<&[GraphValue]>,
    work: &RefCell<impl WorkControl>,
) -> Result<(Vec<GraphHybridHit>, u64), BeaconError> {
    search.execute_filtered(
        eligible,
        policy(),
        &db.snapshot,
        db.frontier().unwrap(),
        work,
        Scan::Metered,
        |_| Ok(true),
        |_| true,
        |_| true,
        |_| true,
    )
}

#[test]
fn native_stage_ignores_disconnected_incidence_for_all_modality_combinations() {
    let ((), report) = run_async_under_lab(0xbeac_3701, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let sparse = fixture(&c.commit(), 0, false).await;
        let dense = fixture(&c.commit(), 1024, false).await;
        let eligible = [GraphValue::Vertex(VId(3))];
        for (vector, text) in [(false, false), (false, true), (true, false), (true, true)] {
            let search = search(vector, text);
            let work = RefCell::new(WorkBudget::new(1_000_000));
            let (expected, scratch) = execute(&search, &sparse, Some(&eligible), &work).unwrap();
            let spent = 1_000_000 - work.into_inner().remaining();
            assert_eq!(scratch, 1);
            assert_eq!(expected.len(), 1);
            let hit = &expected[0];
            assert_eq!(hit.id, VId(3));
            assert_eq!(hit.graph_hops, Some(2));
            assert_eq!(hit.graph_rank.map(|rank| rank.get()), Some(1));
            assert_eq!(hit.vector_rank.map(|rank| rank.get()), vector.then_some(1));
            assert_eq!(hit.text_rank.map(|rank| rank.get()), text.then_some(1));
            // The exact allowance from the sparse source also admits the
            // graph containing 1024 unreachable edges. No elapsed-time claim.
            let work = RefCell::new(WorkBudget::new(spent));
            assert_eq!(
                execute(&search, &dense, Some(&eligible), &work).unwrap().0,
                expected
            );
            assert_eq!(work.into_inner().remaining(), 0);
            let short = RefCell::new(WorkBudget::new(spent - 1));
            assert!(matches!(
                execute(&search, &dense, Some(&eligible), &short),
                Err(BeaconError::WorkBudgetExceeded)
            ));
            // Mutation control: selecting the old whole-graph execution body
            // must not fit that same allowance, even though its answer agrees.
            let work = RefCell::new(WorkBudget::new(spent));
            let control = crate::query::beacon::graph::evaluate_filtered(
                &dense.snapshot,
                dense.frontier().unwrap(),
                &search.options,
                search.query(),
                search.expansion(),
                &work,
                Scan::Metered,
                |_| Ok(true),
                |_| true,
                |_| true,
                |_| true,
                || Ok(()),
                |id| id == VId(3),
            );
            assert!(matches!(control, Err(BeaconError::WorkBudgetExceeded)));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(TEXT)),
        (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
        _ => None,
    }
}

fn statement(modalities: &str) -> String {
    format!(
        "CALL hybrid.search({modalities} seeds => [1], max_hops => 2, \
        eligible => $eligible, k => 1, candidates => 1, graph_candidates => 1, \
        graph_weight => 100) YIELD node, graph_hops, vector_rank, text_rank \
        RETURN node.x AS x, graph_hops, vector_rank, text_rank"
    )
}

fn params(ids: &[i64]) -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "eligible",
            ids.iter()
                .map(|id| GraphValue::Scalar(CanonicalScalar::Int(*id)))
                .collect(),
        )
        .unwrap()
}

fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!("hybrid read returned a write"),
    }
}

fn expected(vector: bool, text: bool) -> Vec<Vec<QueryValue>> {
    vec![
        vec![Some(3), Some(2), vector.then_some(1), text.then_some(1)]
            .into_iter()
            .map(|v| {
                QueryValue::Value(GraphValue::Scalar(
                    v.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                ))
            })
            .collect(),
    ]
}

#[test]
fn query_and_authorized_prepared_calls_share_filtered_indexed_semantics() {
    let ((), report) = run_async_under_lab(0xbeac_3702, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let clean = fixture(&c.commit(), 16, false).await;
        let full = fixture(&c.commit(), 16, true).await;
        let authority = Authority::new(
            AuthKey::from_seed(3702),
            NS,
            "native-hybrid",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 1000,
                max_work: 10_000_000,
                max_rows: 10,
            },
        );
        grant.labels = Scope::only([LabelId(1)]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([TEXT, X]);
        let token = authority.issue_at(&grant, 100).unwrap();
        let mut session = full
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                symbols,
                policy(),
                || 100,
            )
            .unwrap();
        for (modalities, vector, text) in [
            ("", false, false),
            ("text => 'graph', text_property => 'body',", false, true),
            ("vector => [0], vector_properties => ['x'],", true, false),
            (
                "text => 'graph', text_property => 'body', vector => [0], vector_properties => ['x'],",
                true,
                true,
            ),
        ] {
            let statement = statement(modalities);
            assert_eq!(
                rows(
                    clean
                        .query(&c.query(), &statement, &params(&[3]), symbols, policy())
                        .unwrap()
                ),
                expected(vector, text)
            );
            let prepared = session
                .prepare(&c.query(), &statement, &params(&[3]))
                .unwrap();
            assert_eq!(
                rows(
                    session
                        .execute(&c.query(), &prepared, &params(&[90, 3, 3]))
                        .unwrap()
                ),
                expected(vector, text)
            );
            assert!(
                rows(
                    session
                        .execute(&c.query(), &prepared, &params(&[]))
                        .unwrap()
                )
                .is_empty()
            );
            assert!(
                session
                    .execute(&c.query(), &prepared, &params(&[-1]))
                    .is_err()
            );
            assert_eq!(
                rows(
                    session
                        .execute(&c.query(), &prepared, &params(&[3]))
                        .unwrap()
                ),
                expected(vector, text)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_native_pin_keeps_historical_edges_but_checks_live_expiry() {
    let ((), report) = run_async_under_lab(0xbeac_3703, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 0, false).await;
        let authority = Authority::new(
            AuthKey::from_seed(3703),
            NS,
            "native-hybrid",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 1000,
                max_work: 10_000_000,
                max_rows: 10,
            },
        );
        grant.labels = Scope::only([LabelId(1)]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([TEXT, X]);
        let token = authority.issue_at(&grant, 100).unwrap();
        let now = std::sync::atomic::AtomicU64::new(100);
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                symbols,
                policy(),
                || now.load(std::sync::atomic::Ordering::Relaxed),
            )
            .unwrap();
        let text = statement("");
        let parameters = params(&[3]);
        let prepared = session.prepare(&c.query(), &text, &parameters).unwrap();
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(2));
        changes.set_vertex_property(VId(3), X, Some(CanonicalScalar::Int(33)));
        db.write(&c.commit(), changes).await.unwrap();
        db.compact(&c.commit()).await.unwrap();
        assert!(
            rows(
                db.query(&c.query(), &text, &parameters, symbols, policy())
                    .unwrap()
            )
            .is_empty()
        );
        drop(db);
        assert_eq!(
            rows(session.execute(&c.query(), &prepared, &parameters).unwrap()),
            expected(false, false)
        );
        now.store(1001, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            session.execute(&c.query(), &prepared, &parameters),
            Err(crate::QueryError::Authorization(
                fgdb_warden::Error::Expired
            ))
        ));
        assert!(session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_native_indexed_checkpoint_can_cancel_even_when_filter_rejects_all() {
    struct Cut {
        calls: usize,
        stop: usize,
    }
    impl WorkControl for Cut {
        fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
            if self.calls == self.stop {
                return Err(BeaconError::Cancelled);
            }
            self.calls += 1;
            if self.calls == self.stop {
                Err(BeaconError::Cancelled)
            } else {
                Ok(())
            }
        }
    }
    let ((), report) = run_async_under_lab(0xbeac_3704, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&c.commit(), 0, false).await;
        let selected = [GraphValue::Vertex(VId(3))];
        for modalities in [(false, false), (true, true)] {
            let search = search(modalities.0, modalities.1);
            for eligible in [&[][..], &selected[..]] {
                let work = RefCell::new(Cut {
                    calls: 0,
                    stop: usize::MAX,
                });
                let rows = execute(&search, &db, Some(eligible), &work).unwrap().0;
                assert_eq!(rows.len(), usize::from(!eligible.is_empty()));
                let calls = work.into_inner().calls;
                for stop in 1..=calls {
                    let work = RefCell::new(Cut { calls: 0, stop });
                    assert!(
                        matches!(
                            execute(&search, &db, Some(eligible), &work),
                            Err(BeaconError::Cancelled)
                        ),
                        "cut {stop}"
                    );
                    assert_eq!(work.into_inner().calls, stop);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn cold_view(db: &Database<MemVfs>) -> crate::EmbeddedReadView {
    crate::EmbeddedReadView {
        snapshot: std::sync::Arc::clone(&db.snapshot),
        beacon_indexes: Default::default(),
    }
}

#[test]
fn current_text_call_reuses_registered_corpus_with_measured_probe_allowance() {
    let ((), report) = run_async_under_lab(0xbeac_3710, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db =
            Database::open_memory(&c.commit(), DatabaseKeys::new([0x73; 32], NS, [0x75; 32]))
                .await
                .unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 0..512 {
            batch.create_vertex(
                VId(id as u128),
                vec![LabelId(1)],
                vec![
                    (
                        TEXT,
                        CanonicalScalar::ucs_basic_text(if id == 0 {
                            "needle background"
                        } else {
                            "background"
                        })
                        .unwrap(),
                    ),
                    (X, CanonicalScalar::Int(id as i64)),
                ],
            );
        }
        db.write(&c.commit(), batch).await.unwrap();
        let statement = "CALL hybrid.search(text => 'needle', text_property => 'body', \
            candidates => 1, k => 1) YIELD node, score, text_rank, text_score \
            RETURN node.x AS x, score, text_rank, text_score";
        let params = GqlParameters::new();
        let crate::PreparedNativeRead::Set(prepared) =
            crate::PreparedNativeRead::prepare(statement, &params, symbols).unwrap()
        else {
            panic!("hybrid CALL must prepare as a relational source");
        };
        let bound = prepared.bind_parameters(&params).unwrap();
        let cold = db
            .execute_graph_set_governed(&c.query(), &bound, policy())
            .unwrap();
        assert_eq!(cold.value.len(), 1);
        assert!(cold.rows.snapshot_records >= 512);
        let expected = rows(
            db.query(&c.query(), statement, &params, symbols, policy())
                .unwrap(),
        );
        let cold_source = cold_view(&db);
        db.register_beacon_index(
            &c.query(),
            &Options::text(TEXT),
            fgdb_beacon::read::ReadPolicy::default(),
        )
        .unwrap();
        let warm = db
            .execute_graph_set_governed(&c.query(), &bound, policy())
            .unwrap();
        assert_eq!(warm.value, cold.value);
        assert_eq!(
            warm.rows.snapshot_records, 0,
            "no vertex corpus is revisited"
        );
        assert!(warm.evaluator.work_units < cold.evaluator.work_units);
        let mut tight = policy();
        tight.evaluator.max_work_units = warm.evaluator.work_units;
        for _ in 0..2 {
            let repeated = db
                .execute_graph_set_governed(&c.query(), &bound, tight)
                .unwrap();
            assert_eq!(repeated.value, warm.value);
            assert_eq!(repeated.evaluator.work_units, warm.evaluator.work_units);
            assert_eq!(
                rows(
                    db.query(&c.query(), statement, &params, symbols, tight)
                        .unwrap()
                ),
                expected
            );
        }
        // Retain the independent source before registration. A newly
        // acquired read view shares the same immutable query-only pins.
        assert!(
            cold_source
                .execute_graph_set_governed(&c.query(), &bound, tight)
                .is_err()
        );
        assert_eq!(
            db.read_session()
                .unwrap()
                .execute_graph_set_governed(&c.query(), &bound, tight)
                .unwrap(),
            warm,
        );
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn registered_full_corpus_cannot_change_authorized_scores_or_unmask_properties() {
    let ((), report) = run_async_under_lab(0xbeac_3711, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut clean = fixture(&c.commit(), 0, false).await;
        let mut full = fixture(&c.commit(), 0, true).await;
        let mut definition = Options::text(TEXT);
        definition.projection.vector = vec![X];
        definition.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
        // No label selector: the maintained corpus includes the hidden row.
        // These are the same projections requested by the calls below.
        full.register_beacon_index(
            &c.query(),
            &definition,
            fgdb_beacon::read::ReadPolicy::default(),
        )
        .unwrap();
        let authority = Authority::new(
            AuthKey::from_seed(3711),
            NS,
            "maintained-hybrid",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 1000,
                max_work: 10_000_000,
                max_rows: 10,
            },
        );
        grant.labels = Scope::only([LabelId(1)]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([TEXT, X]);
        let token = authority.issue_at(&grant, 100).unwrap();
        let mut session = full
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                symbols,
                policy(),
                || 100,
            )
            .unwrap();
        let call = |modalities: &str| {
            format!(
                "CALL hybrid.search({modalities} eligible => $eligible, candidates => 1, k => 1) \
             YIELD node, score, vector_rank, text_rank, vector_distance, text_score \
             RETURN node.x AS x, score, vector_rank, text_rank, vector_distance, text_score"
            )
        };
        for (modalities, has_text) in [
            ("text => 'graph', text_property => 'body',", true),
            ("vector => [0], vector_properties => ['x'],", false),
            (
                "text => 'graph', text_property => 'body', vector => [0], vector_properties => ['x'],",
                true,
            ),
        ] {
            let statement = call(modalities);
            let expected = rows(
                clean
                    .query(&c.query(), &statement, &params(&[3]), symbols, policy())
                    .unwrap(),
            );
            assert_eq!(expected.len(), 1);
            let prepared = session
                .prepare(&c.query(), &statement, &params(&[90, 3]))
                .unwrap();
            assert_eq!(
                rows(
                    session
                        .execute(&c.query(), &prepared, &params(&[90, 3]))
                        .unwrap()
                ),
                expected
            );
            if has_text {
                // A post-filtered global corpus has different BM25 statistics
                // despite selecting the same visible endpoint. This mutation
                // control makes the authorization comparison non-vacuous.
                assert_ne!(
                    rows(
                        full.query(&c.query(), &statement, &params(&[3]), symbols, policy(),)
                            .unwrap()
                    ),
                    expected
                );
            }
        }
        drop(session);

        let mut remove = WriteBatch::new(R);
        for id in [1, 2, 3, 9] {
            remove.set_vertex_property(VId(id), TEXT, None);
        }
        clean.write(&c.commit(), remove).await.unwrap();
        grant.properties = Scope::only([X]);
        let token = authority.issue_at(&grant, 100).unwrap();
        let mut session = full
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                symbols,
                policy(),
                || 100,
            )
            .unwrap();
        let statement = call(
            "text => 'graph', text_property => 'body', vector => [0], vector_properties => ['x'],",
        );
        let expected = rows(
            clean
                .query(&c.query(), &statement, &params(&[3]), symbols, policy())
                .unwrap(),
        );
        let prepared = session
            .prepare(&c.query(), &statement, &params(&[90, 3]))
            .unwrap();
        assert_eq!(
            rows(
                session
                    .execute(&c.query(), &prepared, &params(&[90, 3]))
                    .unwrap()
            ),
            expected,
            "the maintained text lane cannot supply a masked property"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn maintained_graph_symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Doc") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => symbols(kind, name),
    }
}

const MAINTAINED_GRAPH_CALL: &str = "CALL hybrid.search(text => 'graph', text_property => 'body', \
     vector => [0], vector_properties => ['x'], label => 'Doc', \
     seeds => [1], relation => 'R', max_hops => 2, eligible => $eligible, \
     candidates => 1, graph_candidates => 1, graph_weight => 100, k => 1) \
     YIELD node, score, vector_rank, text_rank, graph_rank, graph_hops, vector_distance, text_score \
     RETURN node.x AS x, score, vector_rank, text_rank, graph_rank, graph_hops, vector_distance, text_score";

#[test]
fn maintained_indexed_graph_and_call_keep_exact_cut_and_expansion_semantics() {
    let ((), report) = run_async_under_lab(0xbeac_3712, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        for retain_graph in [false, true] {
            let mut db = fixture(&c.commit(), 8, false).await;
            let mut other = WriteBatch::new(RelationId(99));
            other.add_edge(EId(60), VId(1), VId(9), vec![]);
            db.write(&c.commit(), other).await.unwrap();
            let definition = search(true, true).options;
            if retain_graph {
                db.register_beacon_graph_index(
                    &c.query(),
                    &definition,
                    fgdb_beacon::read::ReadPolicy::default(),
                )
                .unwrap();
            } else {
                db.register_beacon_index(
                    &c.query(),
                    &definition,
                    fgdb_beacon::read::ReadPolicy::default(),
                )
                .unwrap();
            }
            let parameters = params(&[3]);
            let prepared = crate::PreparedNativeRead::prepare(
                MAINTAINED_GRAPH_CALL,
                &parameters,
                maintained_graph_symbols,
            )
            .unwrap();
            let compare = |db: &Database<MemVfs>| {
                let view = cold_view(db);
                for (vector, text) in [(false, false), (true, false), (false, true), (true, true)] {
                    for (direction, relation, hops) in [
                        (ExpansionDirection::Outgoing, Some(R), 2),
                        (ExpansionDirection::Undirected, None, 1),
                        (ExpansionDirection::Incoming, Some(R), 2),
                    ] {
                        let mut request = search(vector, text);
                        request.direction = direction;
                        request.relation = relation;
                        request.max_hops = hops;
                        let expected = view
                            .beacon_search_graph_indexed_filtered(
                                &c.query(),
                                &request.options,
                                request.query(),
                                request.expansion(),
                                |id| id == VId(3) || id == VId(9),
                            )
                            .unwrap();
                        assert_eq!(
                            db.beacon_search_graph_indexed_filtered(
                                &c.query(),
                                &request.options,
                                request.query(),
                                request.expansion(),
                                |id| id == VId(3) || id == VId(9),
                            )
                            .unwrap(),
                            expected,
                            "vector={vector}, text={text}, direction={direction:?}, relation={relation:?}",
                        );
                    }
                }
                assert_eq!(
                    rows(
                        db.query(
                            &c.query(),
                            MAINTAINED_GRAPH_CALL,
                            &parameters,
                            maintained_graph_symbols,
                            policy(),
                        )
                        .unwrap()
                    ),
                    rows(
                        prepared
                            .execute_in_view(&view, &c.query(), &parameters, policy())
                            .unwrap()
                    )
                );
            };
            compare(&db);
            let old = db.read_session().unwrap();
            let mut request = search(true, true);
            request.relation = Some(R);
            let historical = old
                .beacon_search_graph_indexed_filtered(
                    &c.query(),
                    &request.options,
                    request.query(),
                    request.expansion(),
                    |id| id == VId(3),
                )
                .unwrap();
            assert_eq!(historical.len(), 1);
            assert_eq!(historical[0].graph_hops, Some(2));

            let mut changes = WriteBatch::new(R);
            changes.delete_edge(EId(2));
            changes.add_edge(EId(50), VId(1), VId(3), vec![]);
            changes.set_vertex_property(VId(3), X, Some(CanonicalScalar::Int(30)));
            changes.set_vertex_property(
                VId(3),
                TEXT,
                Some(CanonicalScalar::ucs_basic_text("graph storage").unwrap()),
            );
            changes.set_vertex_label(VId(2), LabelId(1), false);
            db.write(&c.commit(), changes).await.unwrap();
            compare(&db);
            let mut historical_options = request.options.clone();
            historical_options.as_of = Some(old.frontier());
            assert_eq!(
                db.beacon_search_graph_indexed_filtered(
                    &c.query(),
                    &historical_options,
                    request.query(),
                    request.expansion(),
                    |id| id == VId(3),
                )
                .unwrap(),
                historical,
                "current maintained lanes must never join historical graph edges",
            );

            let mut changes = WriteBatch::new(R);
            changes.delete_vertex(VId(3));
            changes.add_edge(EId(51), VId(1), VId(9), vec![]);
            db.write(&c.commit(), changes).await.unwrap();
            compare(&db);
            db.compact(&c.commit()).await.unwrap();
            compare(&db);
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn maintained_indexed_graph_keeps_source_quotas_work_and_live_refusal() {
    use fgdb_beacon::read::ReadError as SearchError;
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;

    let ((), report) = run_async_under_lab(0xbeac_3713, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 0, false).await;
        let parameters = params(&[3]);
        let crate::PreparedNativeRead::Set(prepared) = crate::PreparedNativeRead::prepare(
            MAINTAINED_GRAPH_CALL,
            &parameters,
            maintained_graph_symbols,
        )
        .unwrap() else {
            panic!("hybrid CALL must prepare as a relational source");
        };
        let bound = prepared.bind_parameters(&parameters).unwrap();
        let cold = db
            .execute_graph_set_governed(&c.query(), &bound, policy())
            .unwrap();
        assert_eq!(cold.value.len(), 1);
        let cold_source = cold_view(&db);
        let request = search(true, true);
        db.register_beacon_index(
            &c.query(),
            &request.options,
            fgdb_beacon::read::ReadPolicy::default(),
        )
        .unwrap();
        let warm = db
            .execute_graph_set_governed(&c.query(), &bound, policy())
            .unwrap();
        assert_eq!(warm.value, cold.value);
        assert_eq!(
            warm.rows, cold.rows,
            "the selected directory still admits every source vertex"
        );
        assert!(warm.evaluator.work_units < cold.evaluator.work_units);
        let mut tight = policy();
        tight.evaluator.max_work_units = warm.evaluator.work_units;
        assert_eq!(
            db.execute_graph_set_governed(&c.query(), &bound, tight)
                .unwrap()
                .value,
            warm.value,
        );
        assert!(
            cold_source
                .execute_graph_set_governed(&c.query(), &bound, tight)
                .is_err()
        );
        assert_eq!(
            db.read_session()
                .unwrap()
                .execute_graph_set_governed(&c.query(), &bound, tight)
                .unwrap()
                .value,
            warm.value,
        );

        let view = cold_source;
        for case in 0..6 {
            let mut options = request.options.clone();
            let mut expansion = request.expansion();
            match case {
                0 => options.policy.max_staging_rows = 0,
                1 => expansion.limits.max_vertices = 3,
                2 => expansion.limits.max_visited_vertices = 1,
                3 => expansion.limits.max_input_edges = 0,
                4 => expansion.limits.max_seed_ids = 0,
                5 => expansion.limits.max_source_scratch = 0,
                _ => unreachable!(),
            }
            let expected = view
                .beacon_search_graph_indexed_filtered(
                    &c.query(),
                    &options,
                    request.query(),
                    expansion,
                    |_| false,
                )
                .unwrap_err();
            let actual = db
                .beacon_search_graph_indexed_filtered(
                    &c.query(),
                    &options,
                    request.query(),
                    expansion,
                    |_| false,
                )
                .unwrap_err();
            assert_eq!(
                format!("{actual:?}"),
                format!("{expected:?}"),
                "source quota {case}"
            );
        }

        let cx = c.query();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let measured = cx.with_checkpoint_probe(Arc::clone(&probe));
        let expected = db
            .beacon_search_graph_indexed_filtered(
                &measured,
                &request.options,
                request.query(),
                request.expansion(),
                |id| id == VId(3),
            )
            .unwrap();
        assert_eq!(expected.len(), 1);
        let calls = probe.calls();
        for stop in 1..=calls {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
            assert!(
                matches!(
                    db.beacon_search_graph_indexed_filtered(
                        &interrupted,
                        &request.options,
                        request.query(),
                        request.expansion(),
                        |id| id == VId(3),
                    ),
                    Err(SearchError::Interrupted(_))
                ),
                "graph checkpoint {stop}"
            );
            assert_eq!(probe.calls(), stop);
        }
        assert_eq!(
            db.beacon_search_graph_indexed_filtered(
                &cx,
                &request.options,
                request.query(),
                request.expansion(),
                |id| id == VId(3),
            )
            .unwrap(),
            expected,
        );
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
