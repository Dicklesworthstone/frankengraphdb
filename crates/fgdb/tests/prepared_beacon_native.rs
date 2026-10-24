//! Public prepared retrieval laws over real committed, capability-masked data.
//! Run independently of the monolithic library test binary. Ordinary execution
//! supplies a live semantic oracle; exact allowances detect a second scan.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, QueryResult, QueryValue, WriteBatch};
use fgdb_beacon::read::ReadOptions;
use fgdb_beacon::{DistanceMetric, HnswConfig};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphExactAverage, GraphSymbol, GraphSymbolKind,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
use std::sync::Arc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x85; 32]);
const NODE: LabelId = LabelId(1);
const BODY: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const NAME: PropertyKeyId = PropertyKeyId(3);
const SECRET: PropertyKeyId = PropertyKeyId(99);
const HIGH: VId = VId(u128::MAX);
const NOW: u64 = 100;
const CALL: &str = "CALL hybrid.search(text => 'red', text_property => 'body', \
    vector => [$x], vector_properties => ['x'], label => 'Node', eligible => $eligible, \
    candidates => 1, k => 1) YIELD node, vector_rank, text_rank ";

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn scalar(value: CanonicalScalar) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(value))
}

fn catalog(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(NODE)),
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(BODY)),
        (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}

fn options() -> ReadOptions<PropertyKeyId, LabelId> {
    let mut options = ReadOptions::text(BODY);
    options.vertex_label = Some(NODE);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}

fn policy(records: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(records, 16, 1_000_000, 100_000)
}

fn grant(nodes: u64) -> Grant {
    let mut grant = Grant::read_only(
        "main",
        100_000,
        QueryLimits {
            max_nodes: nodes,
            max_work: 10_000_000,
            max_rows: 16,
        },
    );
    grant.labels = Scope::only([NODE]);
    grant.properties = Scope::only([BODY, X, NAME]);
    grant
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0x85), NS, "graph", SchemaEpoch(0), 1).unwrap()
}

fn parameters(eligible: &[VId]) -> GqlParameters {
    GqlParameters::new()
        .with_int64("x", 0)
        .unwrap()
        .with_list(
            "eligible",
            eligible.iter().copied().map(GraphValue::Vertex).collect(),
        )
        .unwrap()
}

fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Write { .. } => panic!("retrieval returned a write"),
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x86; 32], NS, [0x87; 32])
}

async fn database(cx: &CommitCx) -> (Database<MemVfs>, MemVfs) {
    let vfs = MemVfs::new().unwrap();
    let dir = vfs.database_dir();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), &dir, keys())
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, x, name) in [(VId(1), 0, "one"), (VId(2), 1, "two"), (HIGH, 2, "high")] {
        batch.create_vertex(
            id,
            vec![NODE, LabelId(99)],
            vec![
                (BODY, text("red graph")),
                (X, CanonicalScalar::Int(x)),
                (NAME, text(name)),
                (SECRET, text("private")),
            ],
        );
    }
    // Hidden malformed input must not reach either retrieval projector.
    batch.create_vertex(
        VId(90),
        vec![LabelId(99)],
        vec![
            (BODY, CanonicalScalar::Bool(true)),
            (X, text("not a coordinate")),
            (NAME, text("hidden")),
        ],
    );
    batch.add_edge(EId(1), VId(1), HIGH, vec![]);
    db.write(cx, batch).await.unwrap();
    // Creation seeds derived indexes for publication. Reopening its actual
    // checkpoint leaves both lazy indexes unbuilt, as a cold reader sees them.
    drop(db);
    let db = Database::open_with_vfs(cx, vfs.clone(), &dir, keys())
        .await
        .unwrap();
    assert_eq!(db.index_maintenance_work().unwrap().1, 0);
    (db, vfs)
}

#[test]
fn prepared_properties_labels_bags_aliases_and_aggregates_match_ordinary_execution() {
    let ((), report) = run_async_under_lab(0xbeac_6501, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let (db, _) = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(100), NOW).unwrap();
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &issuer,
                &token,
                "main",
                catalog,
                policy(100),
                || NOW,
            )
            .unwrap();
        let index = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        let params = parameters(&[HIGH]);
        for tail in [
            "RETURN node.name AS name, node.secret AS hidden, labels(node) AS labels",
            "UNWIND [1,1,2] AS copy RETURN node, node.name AS name, copy ORDER BY copy",
            "MATCH (node:Node) RETURN node.name AS name, node.secret AS hidden",
            "WITH node AS hit RETURN hit.name AS name",
            "WITH node.x AS value RETURN COUNT(*) AS n, SUM(value) AS total, AVG(value) AS mean",
            "WITH node AS hit WITH hit.secret AS hidden RETURN COUNT(hidden) AS n",
            "RETURN node.name AS name LIMIT 0",
        ] {
            let query = format!("{CALL}{tail}");
            let prepared = session.prepare(&c.query(), &query, &params).unwrap();
            let expected = session
                .execute(&c.query(), &prepared, &params)
                .unwrap_or_else(|error| panic!("ordinary {query}: {error:?}"));
            let actual = session
                .execute_beacon_index(&c.query(), &index, &prepared, &params)
                .unwrap();
            assert_eq!(actual, expected, "{query}");
            let actual = rows(actual);
            if tail.starts_with("UNWIND") {
                assert_eq!(actual.len(), 3);
                assert_eq!(actual[0], actual[1], "duplicate occurrences survive");
            } else if tail.contains("LIMIT 0") {
                assert!(actual.is_empty());
            } else {
                assert_eq!(actual.len(), 1, "{query}");
            }
            if tail.starts_with("RETURN node.name") && !actual.is_empty() {
                assert_eq!(actual[0][0], scalar(text("high")));
                assert_eq!(actual[0][1], scalar(CanonicalScalar::Null));
                assert_eq!(
                    actual[0][2],
                    QueryValue::Value(GraphValue::List(
                        vec![GraphValue::Scalar(text("Node"))].into()
                    ))
                );
            } else if tail.starts_with("WITH node.x") {
                assert_eq!(
                    actual[0],
                    vec![
                        QueryValue::Count(1),
                        QueryValue::Integer(2),
                        QueryValue::Average(GraphExactAverage::new(2, 1).unwrap()),
                    ]
                );
            } else if tail.contains("hit.secret") {
                assert_eq!(actual[0], vec![QueryValue::Count(0)]);
            }
        }
        let branch = format!("{CALL}RETURN node.name AS name");
        let query = format!("{branch} UNION ALL {branch}");
        let prepared = session.prepare(&c.query(), &query, &params).unwrap();
        let expected = session.execute(&c.query(), &prepared, &params).unwrap();
        let actual = session
            .execute_beacon_index(&c.query(), &index, &prepared, &params)
            .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(rows(actual), vec![vec![scalar(text("high"))]; 2]);
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_prepared_property_paths_never_build_global_indexes_and_preserve_cancellation() {
    let ((), report) = run_async_under_lab(0xbeac_6505, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        for tail in [
            "RETURN node.name AS name",
            "WITH node AS hit RETURN hit.name AS name",
            "WITH node AS hit RETURN hit.name AS name LIMIT 0",
        ] {
            // Each path starts without any ordinary/property read warming the
            // snapshot. Global index maintenance is observable through the
            // public diagnostics, without a test-only production fast path.
            let (db, _) = database(&c.commit()).await;
            let issuer = authority();
            let token = issuer.issue_at(&grant(100), NOW).unwrap();
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    policy(100),
                    || NOW,
                )
                .unwrap();
            let index = session
                .prepare_beacon_index(&c.query(), &options())
                .unwrap();
            let params = parameters(&[HIGH]);
            let prepared = session
                .prepare(&c.query(), &format!("{CALL}{tail}"), &params)
                .unwrap();
            assert_eq!(db.index_maintenance_work().unwrap().1, 0);
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let expected = session
                .execute_beacon_index(
                    &c.query().with_checkpoint_probe(Arc::clone(&probe)),
                    &index,
                    &prepared,
                    &params,
                )
                .unwrap();
            assert_eq!(db.index_maintenance_work().unwrap().1, 0, "{tail}");
            assert_eq!(
                rows(expected.clone()).len(),
                usize::from(!tail.contains("LIMIT 0"))
            );
            let calls = probe.calls();
            assert!(calls > 5);
            for stop in 1..=calls {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let result = session.execute_beacon_index(
                    &c.query().with_checkpoint_probe(Arc::clone(&probe)),
                    &index,
                    &prepared,
                    &params,
                );
                assert!(
                    matches!(
                        result,
                        Err(QueryError::Pattern(GqlQueryError::Interrupted(_)))
                    ),
                    "{tail}, stop={stop}: {result:?}"
                );
                // Session.run performs its one post-action live check even
                // after a source error; the stopped source itself cannot run on.
                assert!(probe.calls() >= stop && probe.calls() <= stop + 1);
                assert!(!session.is_closed());
                assert_eq!(db.index_maintenance_work().unwrap().1, 0);
            }
            assert_eq!(
                session
                    .execute_beacon_index(&c.query(), &index, &prepared, &params)
                    .unwrap(),
                expected
            );
            session.close();
            assert_eq!(c.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_hits_and_properties_keep_the_saved_cut_after_compaction_and_writer_drop() {
    let ((), report) = run_async_under_lab(0xbeac_6502, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let (mut db, vfs) = database(&c.commit()).await;
        let old = db.frontier().unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(HIGH, NAME, Some(text("new name")));
        change.set_vertex_property(HIGH, BODY, Some(text("blue")));
        db.write(&c.commit(), change).await.unwrap();
        let newer = db.frontier().unwrap();
        let mut removal = WriteBatch::new(RelationId(1));
        removal.delete_edge(EId(1));
        removal.delete_vertex(HIGH);
        db.write(&c.commit(), removal).await.unwrap();
        let retired = db.frontier().unwrap();
        // Writes resolve their before-images through derived indexes. Reopen
        // AFTER every write so saved-cut lookups also start genuinely cold.
        let dir = vfs.database_dir();
        drop(db);
        let mut db = Database::open_with_vfs(&c.commit(), vfs, &dir, keys())
            .await
            .unwrap();
        assert_eq!(db.index_maintenance_work().unwrap().1, 0);
        let issuer = authority();
        let token = issuer.issue_at(&grant(100), NOW).unwrap();
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &issuer,
                &token,
                "main",
                catalog,
                policy(100),
                || NOW,
            )
            .unwrap();
        let params = parameters(&[HIGH]);
        let query = format!("{CALL}RETURN node.name AS name, vector_rank, text_rank");
        let prepared = session.prepare(&c.query(), &query, &params).unwrap();
        let alias = session
            .prepare(
                &c.query(),
                &format!("{CALL}WITH node AS hit RETURN hit.name AS name"),
                &params,
            )
            .unwrap();
        let mut cases = Vec::new();
        for (at, name, text_rank) in [
            (fgdb_types::CommitSeq(0), None, CanonicalScalar::Null),
            (old, Some("high"), CanonicalScalar::Int(1)),
            (newer, Some("new name"), CanonicalScalar::Null),
            (retired, None, CanonicalScalar::Null),
        ] {
            let mut historical = options();
            historical.as_of = Some(at);
            let index = session
                .prepare_beacon_index(&c.query(), &historical)
                .unwrap();
            assert_eq!(index.source_sequence(), at);
            let expected = name.map_or_else(Vec::new, |name| {
                vec![vec![
                    scalar(text(name)),
                    scalar(CanonicalScalar::Int(1)),
                    scalar(text_rank),
                ]]
            });
            assert_eq!(
                rows(
                    session
                        .execute_beacon_index(&c.query(), &index, &prepared, &params)
                        .unwrap()
                ),
                expected
            );
            let alias_expected = name.map_or_else(Vec::new, |name| vec![vec![scalar(text(name))]]);
            assert_eq!(
                rows(
                    session
                        .execute_beacon_index(&c.query(), &index, &alias, &params)
                        .unwrap()
                ),
                alias_expected
            );
            cases.push((index, expected, alias_expected));
        }
        assert_eq!(db.index_maintenance_work().unwrap().1, 0);
        db.compact(&c.commit()).await.unwrap();
        drop(db);
        for (index, expected, alias_expected) in cases {
            assert_eq!(
                rows(
                    session
                        .execute_beacon_index(&c.query(), &index, &prepared, &params)
                        .unwrap()
                ),
                expected
            );
            assert_eq!(
                rows(
                    session
                        .execute_beacon_index(&c.query(), &index, &alias, &params)
                        .unwrap()
                ),
                alias_expected
            );
        }
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_identity_reads_refuse_unbound_roots_and_cross_sibling_provenance() {
    let ((), report) = run_async_under_lab(0xbeac_6503, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let (db, _) = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(100), NOW).unwrap();
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &issuer,
                &token,
                "main",
                catalog,
                policy(100),
                || NOW,
            )
            .unwrap();
        let index = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        let params = parameters(&[HIGH]);
        for query in [
            format!("{CALL}MATCH (other:Node) RETURN other.name AS name"),
            format!("{CALL}MATCH (node), (other:Node) RETURN other.name AS name"),
            format!(
                "{CALL}RETURN node.name AS name UNION ALL MATCH (other:Node) RETURN other.name AS name"
            ),
            format!(
                "({CALL}RETURN node.name AS name LIMIT 0) UNION ALL MATCH (node:Node) RETURN node.name AS name"
            ),
            format!("{CALL}MATCH (node)-[]->(other) RETURN other.name AS name LIMIT 0"),
        ] {
            let prepared = session
                .prepare(&c.query(), &query, &params)
                .unwrap_or_else(|error| panic!("prepare {query}: {error:?}"));
            for eligible in [&[HIGH][..], &[][..]] {
                let params = parameters(eligible);
                // The ordinary source still supports these valid graph reads.
                session.execute(&c.query(), &prepared, &params).unwrap();
                assert!(
                    session
                        .execute_beacon_index(&c.query(), &index, &prepared, &params)
                        .is_err(),
                    "{query}, eligible={eligible:?}"
                );
                assert!(!session.is_closed());
            }
        }
        let good = session
            .prepare(
                &c.query(),
                &format!("{CALL}RETURN node.name AS name"),
                &params,
            )
            .unwrap();
        assert_eq!(
            rows(
                session
                    .execute_beacon_index(&c.query(), &index, &good, &params)
                    .unwrap()
            ),
            vec![vec![scalar(text("high"))]]
        );
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_identity_reads_pay_the_saved_population_once_and_share_call_budgets() {
    let ((), report) = run_async_under_lab(0xbeac_6504, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let (db, _) = database(&c.commit()).await;
        let issuer = authority();
        let params = parameters(&[HIGH]);
        let branch = format!("{CALL}RETURN node.name AS name");
        let both = format!("{branch} UNION ALL {branch}");
        // There are exactly three capability-visible source vertices. A
        // returned-hit identity read must not admit that population twice.
        for (records, nodes, two_calls) in [(3, 100, false), (100, 3, false), (6, 6, true)] {
            let token = issuer.issue_at(&grant(nodes), NOW).unwrap();
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    policy(records),
                    || NOW,
                )
                .unwrap();
            let index = session
                .prepare_beacon_index(&c.query(), &options())
                .unwrap();
            let single = session.prepare(&c.query(), &branch, &params).unwrap();
            let double = session.prepare(&c.query(), &both, &params).unwrap();
            assert_eq!(
                rows(
                    session
                        .execute_beacon_index(&c.query(), &index, &single, &params)
                        .unwrap()
                ),
                vec![vec![scalar(text("high"))]]
            );
            let result = session.execute_beacon_index(&c.query(), &index, &double, &params);
            if two_calls {
                assert_eq!(rows(result.unwrap()), vec![vec![scalar(text("high"))]; 2]);
            } else {
                assert!(result.is_err(), "records={records}, nodes={nodes}");
            }
            assert!(!session.is_closed());
            // Refused statements cannot retain a spent permit or identity set.
            assert!(
                session
                    .execute_beacon_index(&c.query(), &index, &single, &params)
                    .is_ok()
            );
            session.close();
            assert_eq!(c.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn supplied_element_parameters_cannot_bypass_vertex_relation_or_endpoint_scope() {
    let ((), report) = run_async_under_lab(0xbeac_6506, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let (mut db, vfs) = database(&c.commit()).await;
        let mut edges = WriteBatch::new(RelationId(1));
        edges.set_edge_property(EId(1), NAME, Some(text("allowed edge")));
        edges.add_edge(EId(2), VId(1), VId(90), vec![(NAME, text("hidden target"))]);
        edges.add_edge(EId(3), VId(90), HIGH, vec![(NAME, text("hidden source"))]);
        edges.add_edge(EId(4), VId(1), VId(1), vec![(NAME, text("self loop"))]);
        db.write(&c.commit(), edges).await.unwrap();
        let mut denied = WriteBatch::new(RelationId(99));
        denied.add_edge(EId(5), VId(1), HIGH, vec![(NAME, text("hidden relation"))]);
        db.write(&c.commit(), denied).await.unwrap();
        let issuer = authority();
        let mut allowed = grant(100);
        allowed.relations = Scope::only([RelationId(1)]);
        let token = issuer.issue_at(&allowed, NOW).unwrap();
        let params = GqlParameters::new()
            .with_list(
                "elements",
                vec![
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Vertex(VId(90)),
                    GraphValue::Vertex(VId(91)),
                    GraphValue::Edge(EId(1)),
                    GraphValue::Edge(EId(2)),
                    GraphValue::Edge(EId(3)),
                    GraphValue::Edge(EId(4)),
                    GraphValue::Edge(EId(5)),
                    GraphValue::Edge(EId(404)),
                ],
            )
            .unwrap();
        let statements = [
            "UNWIND $elements AS element RETURN element.name AS name",
            "UNWIND $elements AS element WITH element.name AS name ORDER BY name RETURN COUNT(name) AS n",
            "UNWIND $elements AS element WITH element.name AS name RETURN COUNT(name) AS n",
        ];
        let expected = vec![
            [
                Some("one"),
                None,
                None,
                Some("allowed edge"),
                None,
                None,
                Some("self loop"),
                None,
                None,
            ]
            .into_iter()
            .map(|name| vec![scalar(name.map_or(CanonicalScalar::Null, text))])
            .collect::<Vec<_>>(),
            vec![vec![QueryValue::Count(3)]],
            vec![vec![QueryValue::Count(3)]],
        ];
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &issuer,
                &token,
                "main",
                catalog,
                policy(100),
                || NOW,
            )
            .unwrap();
        let prepared: Vec<_> = statements
            .iter()
            .map(|text| session.prepare(&c.query(), text, &params).unwrap())
            .collect();
        // Execute both facades before asserting: the same supplied identities
        // must be masked both in rows and before an aggregate observes them.
        let actual: Vec<_> = prepared
            .iter()
            .map(|query| rows(session.execute(&c.query(), query, &params).unwrap()))
            .collect();
        let nonnull = actual[0]
            .iter()
            .filter(|row| row[0] != scalar(CanonicalScalar::Null))
            .count();
        let QueryValue::Count(aggregate_count) = actual[1][0][0] else {
            panic!("COUNT did not return a count");
        };
        let QueryValue::Count(folded_count) = actual[2][0][0] else {
            panic!("folded COUNT did not return a count");
        };
        assert_eq!(
            (nonnull, aggregate_count, folded_count),
            (3, 3, 3),
            "supplied hidden vertices, relations and endpoints must contribute neither values nor counts"
        );
        assert_eq!(actual, expected);

        // Reopen the same checkpoint for each injection so derived-index
        // promotion cannot move a later run's physical cancellation points.
        // Source lookup errors keep their original cancellation class in row,
        // materialized aggregate and folded aggregate execution, with retry.
        let dir = vfs.database_dir();
        drop(db);
        for (statement, expected) in statements.iter().zip(&expected) {
            let (mut stop, mut calls) = (0, 0);
            loop {
                let reader = Database::open_with_vfs(&c.commit(), vfs.clone(), &dir, keys())
                    .await
                    .unwrap();
                let mut reading = reader
                    .authorized_read_session(
                        &c.query(),
                        &issuer,
                        &token,
                        "main",
                        catalog,
                        policy(100),
                        || NOW,
                    )
                    .unwrap();
                let query = reading.prepare(&c.query(), statement, &params).unwrap();
                let probe = Arc::new(SimulationCheckpointProbe::new((stop != 0).then_some(stop)));
                let result = reading.execute(
                    &c.query().with_checkpoint_probe(Arc::clone(&probe)),
                    &query,
                    &params,
                );
                if stop == 0 {
                    assert_eq!(rows(result.unwrap()), *expected);
                    calls = probe.calls();
                    assert!(calls > 0);
                } else {
                    assert!(
                        matches!(
                            result,
                            Err(QueryError::Pattern(GqlQueryError::Interrupted(_)))
                        ),
                        "{statement}, stop={stop}: {result:?}"
                    );
                    // Ordinary native execution and the retained session
                    // each perform one final validity checkpoint, including
                    // after failure. The element-error latch itself performs
                    // no further source probe or evaluator checkpoint.
                    assert!(
                        probe.calls() >= stop && probe.calls() <= stop + 2,
                        "{statement}, stop={stop}, calls={}",
                        probe.calls()
                    );
                    assert!(!reading.is_closed());
                    assert_eq!(
                        rows(reading.execute(&c.query(), &query, &params).unwrap()),
                        *expected
                    );
                }
                reading.close();
                stop += 1;
                if stop > calls {
                    break;
                }
            }
        }

        // A successor that hides an endpoint must not change the pinned
        // session, and it must hide both the vertex and its incident edges
        // from a new session. A historical predecessor cannot be resurrected.
        let mut db = Database::open_with_vfs(&c.commit(), vfs, &dir, keys())
            .await
            .unwrap();
        let mut hide = WriteBatch::new(RelationId(1));
        hide.set_vertex_label(VId(1), NODE, false);
        db.write(&c.commit(), hide).await.unwrap();
        db.compact(&c.commit()).await.unwrap();
        for (query, expected) in prepared.iter().zip(&expected) {
            assert_eq!(
                rows(session.execute(&c.query(), query, &params).unwrap()),
                *expected
            );
        }
        session.close();
        let mut current = db
            .authorized_read_session(
                &c.query(),
                &issuer,
                &token,
                "main",
                catalog,
                policy(100),
                || NOW,
            )
            .unwrap();
        let hidden = [
            vec![vec![scalar(CanonicalScalar::Null)]; 9],
            vec![vec![QueryValue::Count(0)]],
            vec![vec![QueryValue::Count(0)]],
        ];
        for (text, expected) in statements.iter().zip(hidden) {
            let prepared = current.prepare(&c.query(), text, &params).unwrap();
            assert_eq!(
                rows(current.execute(&c.query(), &prepared, &params).unwrap()),
                expected
            );
        }
        current.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
