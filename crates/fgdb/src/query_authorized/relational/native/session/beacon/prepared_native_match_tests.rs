//! A prepared retrieval hit roots the ordinary authorized graph closure.
//! Exercise the real compiler, index, session permit, and historical source.
use super::*;

const RETRIEVE: &str = "CALL hybrid.search(text => 'graph', text_property => 'body', \
    label => 'Node', eligible => $eligible, candidates => 1, k => 1) YIELD node ";
const SECRET: PropertyKeyId = PropertyKeyId(99);

struct MatchCatalog;

impl GraphSymbolResolver for MatchCatalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
            _ => catalog(kind, name),
        }
    }

    fn reverse_catalog(&self) -> Option<fgdb_gql::ReverseSymbolCatalog> {
        let mut names = fgdb_gql::ReverseSymbolCatalog::new();
        names.insert_label(L, "Node");
        names.insert_relation(R, "R");
        names.insert_property(BODY, "body");
        names.insert_property(X, "x");
        names.insert_property(NAME, "name");
        // Hidden labels, relation types and fields remain deliberately unnamed.
        Some(names)
    }
}

fn arguments(ids: &[VId]) -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "eligible",
            ids.iter().copied().map(GraphValue::Vertex).collect(),
        )
        .unwrap()
}

async fn weighted_database(cx: &CommitCx, noisy: bool) -> Database<MemVfs> {
    let mut db = database(cx, noisy).await;
    let mut properties = WriteBatch::new(R);
    for (id, value) in [(0, 7), (1, 11), (2, 13), (3, 17), (4, 19)] {
        properties.set_edge_property(EId(id), X, Some(CanonicalScalar::Int(value)));
        properties.set_edge_property(EId(id), SECRET, Some(text("hidden edge field")));
    }
    db.write(cx, properties).await.unwrap();
    db
}

#[test]
fn returned_hits_expand_bags_directions_paths_and_masked_element_metadata() {
    let ((), report) = run_async_under_lab(0xbeac_6501, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = weighted_database(&c.commit(), true).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                MatchCatalog,
                host(),
                || NOW,
            )
            .unwrap();
        let index = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        let params = arguments(&[VId(0)]);
        for (tail, count) in [
            (
                "MATCH (node)-[e:R]->(neighbor) RETURN neighbor, neighbor.name, e, e.x, \
                 neighbor.secret, e.secret ORDER BY e",
                2,
            ),
            (
                "MATCH (node)<-[e:R]-(neighbor) RETURN neighbor, neighbor.name, e, e.x ORDER BY e",
                1,
            ),
            (
                "MATCH (node)-[e:R]-(neighbor) RETURN neighbor, neighbor.name, e, e.x ORDER BY e",
                3,
            ),
            (
                "UNWIND [1,1] AS copy MATCH (node)-[e:R]->(neighbor) \
                 RETURN neighbor, e, copy ORDER BY e, copy",
                4,
            ),
            (
                "MATCH p=(node)-[:R*1..2]->(neighbor) \
                 RETURN neighbor, length(p) AS hops ORDER BY neighbor, hops",
                6,
            ),
            (
                "MATCH (node)-[e:R]->(neighbor) WITH neighbor, e \
                 RETURN labels(neighbor) AS labels, properties(neighbor).name AS name, \
                 type(e) AS relation, keys(e) AS keys, properties(e).x AS weight ORDER BY weight",
                2,
            ),
        ] {
            let query = format!("{RETRIEVE}{tail}");
            let prepared = session.prepare(&c.query(), &query, &params).unwrap();
            let expected = rows(session.execute(&c.query(), &prepared, &params).unwrap());
            assert_eq!(expected.len(), count, "{query}");
            let scans = crate::query::authorized::SCANS.with(Cell::get);
            let actual = rows(
                session
                    .execute_beacon_index(&c.query(), &index, &prepared, &params)
                    .unwrap(),
            );
            assert_eq!(actual, expected, "{query}");
            assert_eq!(crate::query::authorized::SCANS.with(Cell::get), scans);
            if tail.contains("neighbor.secret") {
                for row in actual {
                    assert_eq!(&row[4..], &[integer_null(), integer_null()]);
                }
            }
        }
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn integer_null() -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null))
}

#[test]
fn private_expansion_rows_feed_exact_groups_under_one_signed_delivery_row() {
    let ((), report) = run_async_under_lab(0xbeac_6502, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = weighted_database(&c.commit(), true).await;
        let authority = authority();
        let mut allowed = grant();
        allowed.limits.max_rows = 1;
        let token = authority.issue_at(&allowed, NOW).unwrap();
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                MatchCatalog,
                host(),
                || NOW,
            )
            .unwrap();
        let index = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        let params = arguments(&[VId(0)]);
        let query = format!(
            "{RETRIEVE}MATCH (node)-[e:R]->(neighbor) \
             RETURN neighbor.name AS name, COUNT(*) AS n, SUM(e.x) AS weight ORDER BY name"
        );
        let prepared = session.prepare(&c.query(), &query, &params).unwrap();
        let actual = rows(
            session
                .execute_beacon_index(&c.query(), &index, &prepared, &params)
                .unwrap(),
        );
        assert_eq!(
            actual,
            vec![vec![
                QueryValue::Value(GraphValue::Scalar(text("transit"))),
                QueryValue::Count(2),
                QueryValue::Integer(18),
            ]]
        );
        assert_eq!(
            rows(session.execute(&c.query(), &prepared, &params).unwrap()),
            actual
        );
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn match_properties_and_incidence_stay_at_the_index_cut_after_compaction() {
    let ((), report) = run_async_under_lab(0xbeac_6503, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = weighted_database(&c.commit(), true).await;
        let old = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(1), NAME, Some(text("new transit")));
        change.set_edge_property(EId(0), X, Some(CanonicalScalar::Int(700)));
        change.delete_edge(EId(1));
        db.write(&c.commit(), change).await.unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                MatchCatalog,
                host(),
                || NOW,
            )
            .unwrap();
        let mut historical = options();
        historical.as_of = Some(old);
        let index = session
            .prepare_beacon_index(&c.query(), &historical)
            .unwrap();
        let params = arguments(&[VId(0)]);
        let query = format!(
            "{RETRIEVE}MATCH (node)-[e:R]->(neighbor) WITH neighbor, e \
             RETURN properties(neighbor).name AS name, properties(e).x AS weight ORDER BY weight"
        );
        let prepared = session.prepare(&c.query(), &query, &params).unwrap();
        let expected = vec![
            vec![
                QueryValue::Value(GraphValue::Scalar(text("transit"))),
                integer(7),
            ],
            vec![
                QueryValue::Value(GraphValue::Scalar(text("transit"))),
                integer(11),
            ],
        ];
        assert_ne!(
            rows(session.execute(&c.query(), &prepared, &params).unwrap()),
            expected
        );
        db.compact(&c.commit()).await.unwrap();
        drop(db);
        assert_eq!(
            rows(
                session
                    .execute_beacon_index(&c.query(), &index, &prepared, &params)
                    .unwrap()
            ),
            expected
        );
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn incidence_never_lends_uncorrelated_or_stale_root_authority_even_on_empty_inputs() {
    let ((), report) = run_async_under_lab(0xbeac_6504, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = weighted_database(&c.commit(), true).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                MatchCatalog,
                host(),
                || NOW,
            )
            .unwrap();
        let index = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        for eligible in [vec![VId(0)], vec![]] {
            let params = arguments(&eligible);
            for tail in [
                "MATCH (other)-[:R]->(neighbor) RETURN neighbor LIMIT 0",
                "MATCH (node)-[:R]->(neighbor), (other) RETURN other LIMIT 0",
                "RETURN node AS v UNION ALL MATCH (other)-[:R]->(neighbor) RETURN neighbor AS v",
            ] {
                let query = format!("{RETRIEVE}{tail}");
                let prepared = session.prepare(&c.query(), &query, &params).unwrap();
                assert!(session.execute(&c.query(), &prepared, &params).is_ok());
                let scans = crate::query::authorized::SCANS.with(Cell::get);
                assert!(
                    session
                        .execute_beacon_index(&c.query(), &index, &prepared, &params)
                        .is_err(),
                    "{query}, eligible={eligible:?}"
                );
                assert_eq!(crate::query::authorized::SCANS.with(Cell::get), scans);
                assert!(!session.is_closed());
            }
            let outside = params
                .with_list("outside", vec![GraphValue::Vertex(HIGH)])
                .unwrap();
            for tail in [
                "WITH $outside[0] AS node MATCH (node)-[:R]->(neighbor) RETURN neighbor LIMIT 0",
                "RETURN node AS v UNION ALL UNWIND $outside AS node \
                 MATCH (node)-[:R]->(neighbor) RETURN neighbor AS v",
            ] {
                let prepared = session
                    .prepare(&c.query(), &format!("{RETRIEVE}{tail}"), &outside)
                    .unwrap();
                assert!(
                    session
                        .execute_beacon_index(&c.query(), &index, &prepared, &outside)
                        .is_err(),
                    "{tail}, eligible={eligible:?}"
                );
                assert!(!session.is_closed());
            }
        }
        let params = arguments(&[VId(0)])
            .with_list("outside", vec![GraphValue::Edge(EId(u128::MAX))])
            .unwrap();
        let prepared = session
            .prepare(
                &c.query(),
                &format!(
                    "{RETRIEVE}MATCH (node)-[e:R]->(neighbor) WITH $outside[0] AS e \
                     RETURN properties(e) AS fields LIMIT 0"
                ),
                &params,
            )
            .unwrap();
        assert!(
            session
                .execute_beacon_index(&c.query(), &index, &prepared, &params)
                .is_err()
        );
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn call_and_match_share_exact_records_nodes_and_set_sibling_allowances() {
    let ((), report) = run_async_under_lab(0xbeac_6505, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = weighted_database(&c.commit(), true).await;
        let authority = authority();
        let params = arguments(&[VId(0)]);
        let query = format!("{RETRIEVE}MATCH (node)-[e:R]->(neighbor) RETURN neighbor, e");
        for nodes in [5, 6, 12] {
            let mut allowed = grant();
            allowed.limits.max_nodes = nodes;
            let token = authority.issue_at(&allowed, NOW).unwrap();
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    MatchCatalog,
                    host(),
                    || NOW,
                )
                .unwrap();
            let index = session
                .prepare_beacon_index(&c.query(), &options())
                .unwrap();
            let single = session.prepare(&c.query(), &query, &params).unwrap();
            let both = session
                .prepare(&c.query(), &format!("{query} UNION ALL {query}"), &params)
                .unwrap();
            // Four corpus vertices, then two closure vertices and two edges.
            // Parallel edges remain distinct, and the root is charged once
            // by EACH source owner instead of inheriting a fresh permit.
            for records in [7, 8] {
                session.state.as_mut().unwrap().policy =
                    GqlQueryPolicy::new(records, 16, 1_000_000, 100_000);
                let result = session.execute_beacon_index(&c.query(), &index, &single, &params);
                assert_eq!(result.is_ok(), records == 8 && nodes >= 6, "{result:?}");
                assert!(!session.is_closed());
            }
            for records in [15, 16] {
                session.state.as_mut().unwrap().policy =
                    GqlQueryPolicy::new(records, 16, 1_000_000, 100_000);
                let result = session.execute_beacon_index(&c.query(), &index, &both, &params);
                assert_eq!(result.is_ok(), records == 16 && nodes == 12, "{result:?}");
                if let Ok(result) = result {
                    assert_eq!(rows(result).len(), 4);
                }
                assert!(!session.is_closed());
            }
            session.close();
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn match_work_and_scratch_thresholds_ignore_hidden_and_disconnected_topology() {
    let ((), report) = run_async_under_lab(0xbeac_6506, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut thresholds = Vec::new();
        for noisy in [false, true] {
            let db = weighted_database(&c.commit(), noisy).await;
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    MatchCatalog,
                    host(),
                    || NOW,
                )
                .unwrap();
            let index = session
                .prepare_beacon_index(&c.query(), &options())
                .unwrap();
            let params = arguments(&[VId(0)]);
            let prepared = session
                .prepare(
                    &c.query(),
                    &format!("{RETRIEVE}MATCH (node)-[e:R]->(neighbor) RETURN neighbor.name, e.x"),
                    &params,
                )
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
            thresholds.push((work, scratch));
            session.close();
        }
        assert_eq!(thresholds[0], thresholds[1]);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancelled_expansion_and_late_empty_expiry_never_release_a_result_prefix() {
    let ((), report) = run_async_under_lab(0xbeac_6507, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = weighted_database(&c.commit(), true).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for eligible in [vec![VId(0)], vec![]] {
            let samples = Cell::new(0_u64);
            let stop = Cell::new(u64::MAX);
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    MatchCatalog,
                    host(),
                    || {
                        let sample = samples.get();
                        samples.set(sample + 1);
                        if sample >= stop.get() { EXPIRES } else { NOW }
                    },
                )
                .unwrap();
            let index = session
                .prepare_beacon_index(&c.query(), &options())
                .unwrap();
            let params = arguments(&eligible);
            let prepared = session
                .prepare(
                    &c.query(),
                    &format!(
                        "{RETRIEVE}MATCH (node)-[e:R]->(neighbor) WITH neighbor, e \
                         RETURN properties(neighbor).name, properties(e).x"
                    ),
                    &params,
                )
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
                let cancelled = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
                assert!(
                    session
                        .execute_beacon_index(
                            &c.query().with_checkpoint_probe(cancelled.clone()),
                            &index,
                            &prepared,
                            &params,
                        )
                        .is_err(),
                    "cut={cut}, eligible={eligible:?}"
                );
                assert!(cancelled.calls() >= cut);
                assert!(!session.is_closed());
                assert_eq!(
                    session
                        .execute_beacon_index(&c.query(), &index, &prepared, &params)
                        .unwrap(),
                    expected
                );
            }
            if !eligible.is_empty() {
                let bad = session
                    .prepare(
                        &c.query(),
                        &format!(
                            "{RETRIEVE}MATCH (node)-[e:R]->(neighbor) \
                             RETURN 1 / 0 AS invalid LIMIT 0"
                        ),
                        &params,
                    )
                    .unwrap();
                assert!(
                    session
                        .execute_beacon_index(&c.query(), &index, &bad, &params)
                        .is_err()
                );
                assert!(!session.is_closed());
                assert_eq!(
                    session
                        .execute_beacon_index(&c.query(), &index, &prepared, &params)
                        .unwrap(),
                    expected
                );
            }
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
