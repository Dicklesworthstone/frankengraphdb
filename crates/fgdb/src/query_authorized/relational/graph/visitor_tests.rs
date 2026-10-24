//! Plain aggregation must preserve the native binding visitor's occurrence order.
use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GraphAggregateValue, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
};
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x85; 32]);
const P: PropertyKeyId = PropertyKeyId(1);
const H: PropertyKeyId = PropertyKeyId(2);
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(8501), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits {
            max_nodes: 1000,
            max_work: 1_000_000,
            max_rows: 1000,
        },
    );
    grant.labels = Scope::only([LabelId(1)]);
    grant.properties = Scope::only([P]);
    grant.relations = Scope::All;
    grant
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(H)),
        _ => None,
    }
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x15; 32], NS, [0x25; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, value, visible) in [
        (0, Some(7), true),
        (1, Some(3), true),
        (2, Some(999), false),
        (3, Some(7), true),
        (u128::MAX, None, true),
    ] {
        let mut properties = Vec::new();
        if let Some(value) = value {
            properties.push((P, CanonicalScalar::Int(value)));
        }
        properties.push((H, CanonicalScalar::ucs_basic_text("hidden").unwrap()));
        batch.create_vertex(
            VId(id),
            vec![LabelId(if visible { 1 } else { 99 })],
            properties,
        );
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn list(values: &[i64]) -> QueryValue {
    GraphAggregateValue::Value(GraphValue::List(
        values
            .iter()
            .map(|value| GraphValue::Scalar(CanonicalScalar::Int(*value)))
            .collect(),
    ))
}
fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!("read returned a write"),
    }
}

#[test]
fn eager_plain_collect_agrees_with_native_visitation_not_sorted_projection() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let db = runtime.block_on(database(&contexts.commit()));
    let issuer = authority();
    let token = issuer.issue_at(&grant(), 100).unwrap();
    let params = GqlParameters::new();
    let text = "MATCH (n) WHERE n.hidden IS NULL RETURN COLLECT(n.p) AS items,COLLECT(DISTINCT n.p) AS unique_items,COUNT(*) AS n";
    let mut session = db
        .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
        .unwrap();
    let prepared = session.prepare(&cx, text, &params).unwrap();
    let expected = vec![
        list(&[7, 3, 7]),
        list(&[7, 3]),
        GraphAggregateValue::Count(4),
    ];
    assert_eq!(
        rows(session.execute(&cx, &prepared, &params).unwrap()),
        vec![expected.clone()]
    );
    assert_eq!(
        rows(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                text,
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap()
        ),
        vec![expected.clone()]
    );
    let mut cursor = session.stream_aggregate(&cx, &prepared, &params).unwrap();
    assert_eq!(cursor.next().unwrap().unwrap().values(), expected);
    assert!(cursor.next().is_none());

    // Mutation-sensitive negative control: the replaced adapter asks a row
    // query to sort its output before the receiver observes any occurrence.
    let definition = PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&params)
        .unwrap();
    let sorted = authorized(
        &db,
        &cx,
        &issuer,
        &token,
        "main",
        None,
        || 100,
        |snapshot, at, scope, execution| {
            definition
                .execute_with_source_governed(
                    policy(),
                    |pattern: &PreparedGraphPattern<GraphValueRow>, remaining| {
                        pattern_at(snapshot, at, pattern, scope, execution, remaining)
                    },
                    || execution.borrow_mut().checkpoint(),
                )
                .map(|result| result.value)
                .map_err(|error| {
                    aggregate_gql_error(
                        error.map_source(|source| source.map_source(GqlError::Read)),
                    )
                })
        },
    )
    .unwrap();
    assert_ne!(sorted[0].values(), expected);
    assert_eq!(sorted[0].values()[0], list(&[3, 7, 7]));
}

#[test]
fn hidden_group_keys_and_history_do_not_change_visitation_or_delivery_admission() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let commit = contexts.commit();
    let mut db = runtime.block_on(database(&commit));
    let basis = db.frontier().unwrap();
    let issuer = authority();
    let mut g = grant();
    g.limits.max_nodes = 4;
    g.limits.max_rows = 1;
    let token = issuer.issue_at(&g, 100).unwrap();
    let params = GqlParameters::new();
    let text = "MATCH (n) RETURN n.hidden AS key,COLLECT(n.p) AS items,COUNT(*) AS n GROUP BY n.hidden HAVING COUNT(*) = 4 LIMIT 1";
    let expected = vec![
        GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
        list(&[7, 3, 7]),
        GraphAggregateValue::Count(4),
    ];
    assert_eq!(
        rows(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                text,
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap()
        ),
        vec![expected.clone()]
    );
    let mut change = WriteBatch::new(RelationId(1));
    change.set_vertex_label(VId(1), LabelId(1), false);
    change.set_vertex_property(VId(0), P, Some(CanonicalScalar::Int(70)));
    runtime.block_on(db.write(&commit, change)).unwrap();
    runtime.block_on(db.compact(&commit)).unwrap();
    let history = format!(
        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ {} RETURN n.hidden AS key,COLLECT(n.p) AS items,COUNT(*) AS n GROUP BY n.hidden",
        basis.0
    );
    assert_eq!(
        rows(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                &history,
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap()
        ),
        vec![expected]
    );
    let mut fewer = g.clone();
    fewer.limits.max_nodes = 3;
    let denied = issuer.issue_at(&fewer, 100).unwrap();
    assert!(matches!(
        db.query_authorized(
            &cx,
            &issuer,
            &denied,
            "main",
            &format!("{history} LIMIT 0"),
            &params,
            symbols,
            policy(),
            || 100
        ),
        Err(QueryError::Authorization(
            fgdb_warden::Error::LimitExceeded(fgdb_warden::LimitDimension::Nodes)
        ))
    ));
}

#[test]
fn computed_and_paged_relational_inputs_keep_their_own_complete_row_order() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let db = runtime.block_on(database(&contexts.commit()));
    let issuer = authority();
    let token = issuer.issue_at(&grant(), 100).unwrap();
    let params = GqlParameters::new();
    for (text, expected) in [
        (
            "MATCH (n) WITH n.p AS value ORDER BY value DESC NULLS LAST LIMIT 2 RETURN COLLECT(value) AS items",
            list(&[7, 7]),
        ),
        (
            "MATCH (n) WITH n.p AS value ORDER BY value ASC NULLS FIRST LIMIT 3 RETURN COLLECT(value) AS items",
            list(&[3, 7]),
        ),
    ] {
        assert_eq!(
            rows(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    text,
                    &params,
                    symbols,
                    policy(),
                    || 100
                )
                .unwrap()
            ),
            vec![vec![expected]],
            "{text}"
        );
    }
}

fn count_definition(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(RelationId(3))),
        _ => symbols(kind, name),
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

async fn count_database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = database(cx).await;
    let mut edges = WriteBatch::new(RelationId(1));
    for (id, from, to) in [
        (10, 0, 1),
        (11, 0, 1),
        (12, 1, 1),
        (13, 1, 2),
        (14, 2, 3),
        (15, u128::MAX, 0),
    ] {
        edges.add_edge(EId(id), VId(from), VId(to), vec![]);
    }
    db.write(cx, edges).await.unwrap();
    let mut other = WriteBatch::new(RelationId(2));
    other.add_edge(EId(20), VId(0), VId(3), vec![]);
    db.write(cx, other).await.unwrap();
    let mut change = WriteBatch::new(RelationId(1));
    // The new winner is hidden by the label scope. Counting must not revive
    // its earlier visible version or edges through that earlier endpoint.
    change.set_vertex_label(VId(1), LabelId(1), false);
    change.set_vertex_label(VId(1), LabelId(99), true);
    change.set_edge_property(EId(11), P, Some(CanonicalScalar::Int(9)));
    change.delete_edge(EId(15));
    db.write(cx, change).await.unwrap();
    db
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CountEvent {
    Node,
    Poll,
    Source(SourceEvent),
    Evaluate,
}

#[derive(Default)]
struct CountTrace {
    calls: RefCell<Vec<CountEvent>>,
    stop: Option<usize>,
    refused: Cell<bool>,
}

impl CountTrace {
    fn observe(&self, event: CountEvent) -> Result<(), QueryError> {
        assert!(!self.refused.get(), "count called after its first refusal");
        let mut calls = self.calls.borrow_mut();
        calls.push(event);
        if self.stop == Some(calls.len()) {
            self.refused.set(true);
            Err(QueryError::Authorization(
                fgdb_warden::Error::LimitExceeded(fgdb_warden::LimitDimension::Work),
            ))
        } else {
            Ok(())
        }
    }
}

type CountResult = Result<
    fgdb_gql::GqlQueryExecution<GraphAggregateRow>,
    GqlQueryError<GraphAggregateError<GqlError>, QueryError>,
>;

// The incumbent is the actual full Tables admission and original native
// aggregate visitor. It retains and traverses the source, independently of
// the new cardinality fold; both feed the exact same output definition.
fn count_run(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    query: &PreparedGraphAggregate,
    scope: &PlannerPredicates,
    budget: GqlQueryPolicy,
    folded: bool,
    trace: &CountTrace,
) -> CountResult {
    let mut usage = AdmissionUsage::default();
    let mut node = || {
        trace
            .observe(CountEvent::Node)
            .map_err(GqlQueryError::Interrupted)
    };
    let mut poll = || {
        trace
            .observe(CountEvent::Poll)
            .map_err(GqlQueryError::Interrupted)
    };
    let mut control = |event| {
        trace
            .observe(CountEvent::Source(event))
            .map_err(GqlQueryError::Interrupted)?;
        usage.observe::<ReadError, QueryError>(budget, event)
    };
    let result = if folded {
        let source = query.count_summary_source().unwrap();
        count::probe(
            snapshot,
            at,
            source,
            query.input_pattern().plan(),
            scope,
            &mut poll,
        )
        .map_err(|error| {
            error.map_source(|error| GraphAggregateError::Source(GqlError::Read(error)))
        })?;
        let input = count::admit(
            snapshot,
            at,
            source,
            scope,
            &mut node,
            &mut poll,
            &mut control,
        )
        .map_err(|error| {
            error.map_source(|error| GraphAggregateError::Source(GqlError::Read(error)))
        })?;
        query
            .prepare_count_summary(source, input.examined, input.matching)
            .unwrap()
            .execute_governed::<GqlError, QueryError>(
                input.records,
                usage.remaining(budget),
                || trace.observe(CountEvent::Evaluate),
            )
    } else {
        let tables = Tables::admit(
            snapshot,
            query.input_pattern().plan(),
            at,
            false,
            scope,
            &mut node,
            &mut poll,
            &mut control,
        )
        .map_err(|error| {
            error.map_source(|error| GraphAggregateError::Source(GqlError::Read(error)))
        })?;
        query.execute_governed_with_element_reads(
            tables.records,
            tables.vertices.iter().map(|&(vid, _)| vid),
            tables
                .edges
                .iter()
                .map(|&((_, from, relation, to), _)| (from, relation, to)),
            |vid, required| Ok::<_, GqlError>(tables.matches(vid, required, scope)),
            |vid, key| Ok::<_, GqlError>(tables.property(vid, key, scope)),
            |_, _| Ok::<Option<fgdb_types::CanonicalPropertyValue>, GqlError>(None),
            usage.remaining(budget),
            || trace.observe(CountEvent::Evaluate),
        )
    };
    usage.finish(budget, result)
}

fn count_observation(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    query: &PreparedGraphAggregate,
    scope: &PlannerPredicates,
    budget: GqlQueryPolicy,
    folded: bool,
    stop: Option<usize>,
) -> (String, Vec<CountEvent>) {
    let trace = CountTrace {
        stop,
        ..CountTrace::default()
    };
    let result = count_run(snapshot, at, query, scope, budget, folded, &trace);
    (format!("{result:?}"), trace.calls.into_inner())
}

#[test]
fn count_folds_match_full_tables_at_every_cut_budget_and_interruption() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let db = runtime.block_on(count_database(&contexts.commit()));
    let issuer = authority();
    let mut open = grant();
    open.labels = Scope::All;
    open.properties = Scope::All;
    let mut relation = grant();
    relation.relations = Scope::only([RelationId(2)]);
    let mut empty = grant();
    empty.labels = Scope::only([LabelId(500)]);
    for grant in [open, grant(), relation, empty] {
        let token = issuer.issue_at(&grant, 100).unwrap();
        let verified = issuer.verify_at(&token, "main", 100).unwrap();
        let scope = verified.predicates();
        for text in [
            "MATCH (n) RETURN COUNT(*) AS total",
            "MATCH (n) RETURN COUNT(*) AS total,COUNT(*) AS again HAVING COUNT(*) >= 0 LIMIT 1",
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS total",
            "MATCH (a)<-[:R]-(b) RETURN COUNT(*) AS total",
            "MATCH (a)-[r:S]->(b) RETURN COUNT(*) AS total HAVING COUNT(*) > 0",
            "MATCH (a)-[:T]->(b) RETURN COUNT(*) AS total LIMIT 0",
        ] {
            let query = count_definition(text);
            assert!(query.count_summary_source().is_some(), "{text}");
            for cut in 0..=db.frontier().unwrap().0 {
                let at = CommitSeq(cut);
                let trace = CountTrace::default();
                let oracle =
                    count_run(&db.snapshot, at, &query, scope, policy(), false, &trace).unwrap();
                let expected = (format!("Ok({oracle:?})"), trace.calls.into_inner());
                assert_eq!(
                    count_observation(&db.snapshot, at, &query, scope, policy(), true, None),
                    expected,
                    "{text}, cut {cut}, scope {grant:?}"
                );
                for stop in 1..=expected.1.len() {
                    let old = count_observation(
                        &db.snapshot,
                        at,
                        &query,
                        scope,
                        policy(),
                        false,
                        Some(stop),
                    );
                    let new = count_observation(
                        &db.snapshot,
                        at,
                        &query,
                        scope,
                        policy(),
                        true,
                        Some(stop),
                    );
                    assert!(
                        old.0.starts_with("Err("),
                        "refusal must reach the incumbent"
                    );
                    assert_eq!(new, old, "{text}, cut {cut}, stop {stop}");
                    assert_eq!(new.1, expected.1[..stop]);
                }
                let exact = [
                    oracle.rows.snapshot_records,
                    oracle.rows.result_rows,
                    oracle.evaluator.work_units,
                    oracle.evaluator.scratch_entries,
                ];
                for below in [None, Some(0), Some(1), Some(2), Some(3)] {
                    let mut limits = exact;
                    if let Some(dimension) = below {
                        if limits[dimension] == 0 {
                            continue;
                        }
                        limits[dimension] -= 1;
                    }
                    let budget = GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]);
                    let old =
                        count_observation(&db.snapshot, at, &query, scope, budget, false, None);
                    let new =
                        count_observation(&db.snapshot, at, &query, scope, budget, true, None);
                    assert_eq!(new, old, "{text}, cut {cut}, boundary {below:?}");
                    assert_eq!(new.0.starts_with("Ok("), below.is_none());
                }
            }
        }
    }
}

#[test]
fn scoped_count_folds_hide_physical_twins_and_preserve_public_delivery_limits() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let commit = contexts.commit();
    let issuer = authority();
    let mut admitted = grant();
    admitted.limits.max_nodes = 2;
    admitted.limits.max_rows = 1;
    let token = issuer.issue_at(&admitted, 100).unwrap();
    let verified = issuer.verify_at(&token, "main", 100).unwrap();
    let scope = verified.predicates();
    let mut observations = Vec::new();
    for hidden in [false, true] {
        let mut db = runtime
            .block_on(Database::open_memory(
                &commit,
                DatabaseKeys::new([0x15; 32], NS, [0x25; 32]),
            ))
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 0..2 {
            batch.create_vertex(VId(id), vec![LabelId(1)], vec![]);
        }
        batch.add_edge(EId(10), VId(0), VId(1), vec![]);
        batch.add_edge(EId(11), VId(1), VId(1), vec![]);
        if hidden {
            for id in 2..20 {
                batch.create_vertex(VId(id), vec![LabelId(99)], vec![]);
                batch.add_edge(EId(id + 100), VId(0), VId(id), vec![]);
            }
            batch.set_vertex_label(VId(0), LabelId(99), true);
        }
        runtime.block_on(db.write(&commit, batch)).unwrap();
        let mut graph = Vec::new();
        for text in [
            "MATCH (n) RETURN COUNT(*) AS total",
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS total",
        ] {
            let query = count_definition(text);
            let trace = CountTrace::default();
            let result = count_run(
                &db.snapshot,
                db.frontier().unwrap(),
                &query,
                scope,
                policy(),
                true,
                &trace,
            )
            .unwrap();
            let charged: Vec<_> = trace
                .calls
                .into_inner()
                .into_iter()
                .filter(|event| *event != CountEvent::Poll)
                .collect();
            let rows = db
                .execute_graph_aggregate_authorized(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &query,
                    policy(),
                    || 100,
                )
                .unwrap();
            assert_eq!(rows, result.value);
            assert_eq!(rows[0].values()[0], GraphAggregateValue::Count(2));
            graph.push((format!("{result:?}"), charged));
            for (nodes, rows_limit) in [(1, 1), (2, 0)] {
                let mut limited = admitted.clone();
                limited.limits.max_nodes = nodes;
                limited.limits.max_rows = rows_limit;
                let token = issuer.issue_at(&limited, 100).unwrap();
                let result = db.execute_graph_aggregate_authorized(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &query,
                    policy(),
                    || 100,
                );
                assert!(matches!(
                    result,
                    Err(QueryError::Authorization(
                        fgdb_warden::Error::LimitExceeded(_)
                    ))
                ));
            }
            let paged = count_definition(&format!("{text} LIMIT 0"));
            let mut limited = admitted.clone();
            limited.limits.max_nodes = 1;
            limited.limits.max_rows = 0;
            let token = issuer.issue_at(&limited, 100).unwrap();
            assert!(matches!(
                db.execute_graph_aggregate_authorized(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &paged,
                    policy(),
                    || 100,
                ),
                Err(QueryError::Authorization(
                    fgdb_warden::Error::LimitExceeded(fgdb_warden::LimitDimension::Nodes)
                ))
            ));
        }
        observations.push(graph);
    }
    assert_eq!(
        observations[0], observations[1],
        "hidden graph changed charged work or counts"
    );
}
