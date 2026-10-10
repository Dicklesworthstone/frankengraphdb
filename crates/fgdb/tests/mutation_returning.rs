//! `MATCH ... SET/REMOVE/DETACH DELETE ... RETURN ...` stages its effects and
//! produces its rows as one operation. Each row is one selection occurrence;
//! a property read is the element's value AFTER the statement (its own
//! simultaneous assignment, else the pre-statement value), while every
//! right-hand side reads the pre-statement graph. Expected rows are
//! enumerated by hand from `graph()`.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationError, GraphMutationPolicy,
    GraphMutationQueryError, GraphSymbol, GraphSymbolKind, GraphVertexMergeOutcome,
    GraphVertexMergePolicy, GraphVertexUpsertError, GraphVertexUpsertPolicy,
    PreparedGraphMutationQuery, PreparedGraphMutationQueryText, PreparedGraphVertexUpsertQuery,
    PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, QueryCx, TxnCx, VId,
};

const R: RelationId = RelationId(1);
const ITEM: LabelId = LabelId(1);
const SEEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const W: PropertyKeyId = PropertyKeyId(3);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5d; 32],
        DatabaseSecurityNamespaceId([0x5e; 32]),
        [0x5f; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Label, "Seen") => Some(GraphSymbol::Label(SEEN)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
        _ => None,
    }
}
fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 100_000, 100_000_000, 10_000_000)
}
fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(query_policy(), 10_000)
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn prepare(text: &str) -> PreparedGraphMutationQuery {
    PreparedGraphMutationQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}
fn rows(values: Vec<Vec<GraphValue>>) -> Vec<GraphValueRow> {
    values
        .into_iter()
        .map(GraphValueRow::from_owned_values)
        .collect()
}

/// Item vertices 1..4 with p = 1..4 and q = 10 on vertex 4 only. R edges:
/// 11 1->2 (w 5), 12 2->3 (w 6), 13 3->4 (w 7): a chain, so vertices 2 and
/// 3 are both a source and a destination.
fn graph() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for id in 1..=4_i64 {
        let mut properties = vec![(P, CanonicalScalar::Int(id))];
        if id == 4 {
            properties.push((Q, CanonicalScalar::Int(10)));
        }
        batch.create_vertex(VId(id as u128), vec![ITEM], properties);
    }
    for (id, source, destination, w) in [(11, 1, 2, 5), (12, 2, 3, 6), (13, 3, 4, 7)] {
        batch.add_edge(
            EId(id),
            VId(source),
            VId(destination),
            vec![(W, CanonicalScalar::Int(w))],
        );
    }
    batch
}

fn run<T>(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx) -> T) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    let txn = contexts.txn();
    runtime.block_on(test(&commit, &cx, &txn))
}

fn read(db: &Database<fgdb::MemVfs>, cx: &QueryCx, text: &str) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = db
        .query(cx, text, &GqlParameters::new(), symbols, query_policy())
        .expect(text)
    else {
        panic!("{text} is a row query");
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => panic!("{text} returns plain values"),
                })
                .collect()
        })
        .collect()
}

/// Every RHS reads the pre-statement graph; every RETURN read is the
/// element's post-statement value, whichever variable names it. In the
/// chain, vertex 2 is `b` in the first row and `a` in the second.
#[test]
fn returned_properties_are_post_statement_values_per_element() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let (stats, result) = txn
            .execute_graph_mutation_query_governed(
                &mut db,
                cx,
                &prepare(
                    "MATCH (a:Item)-[e:R]->(b:Item) SET b.p = a.p + 100, e.w = e.w * 2 \
                     RETURN a.p AS a, b.p AS b, e.w AS w, a AS source ORDER BY w",
                ),
                policy(),
            )
            .unwrap();
        assert_eq!(stats.effects, 6);
        // b.p = a.p(pre) + 100: vertex 2 <- 101, 3 <- 102, 4 <- 103. Vertex 1
        // is never assigned; vertices 2 and 3 read their new values as `a`.
        assert_eq!(
            result.value,
            rows(vec![
                vec![int(1), int(101), int(10), GraphValue::Vertex(VId(1))],
                vec![int(101), int(102), int(12), GraphValue::Vertex(VId(2))],
                vec![int(102), int(103), int(14), GraphValue::Vertex(VId(3))],
            ])
        );
        assert!(matches!(
            txn.finish(&mut db, commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        // The committed graph agrees with what RETURN reported.
        assert_eq!(
            read(&db, cx, "MATCH (n:Item) RETURN n.p AS p ORDER BY p"),
            vec![vec![int(1)], vec![int(101)], vec![int(102)], vec![int(103)]]
        );
        assert_eq!(
            read(&db, cx, "MATCH (a)-[e:R]->(b) RETURN e.w AS w ORDER BY w"),
            vec![vec![int(10)], vec![int(12)], vec![int(14)]]
        );
    });
}

/// REMOVE reads NULL, an unassigned property reads its old value, DISTINCT
/// and paging shape only the rows, and LIMIT 0 still performs every write.
#[test]
fn paging_never_limits_the_writes_and_remove_reads_null() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let (_, result) = txn
            .execute_graph_mutation_query_governed(
                &mut db,
                cx,
                &prepare("MATCH (n:Item) REMOVE n.q SET n.p = 0 RETURN n.q AS q LIMIT 0"),
                policy(),
            )
            .unwrap();
        assert!(result.value.is_empty());
        let (_, result) = txn
            .execute_graph_mutation_query_governed(
                &mut db,
                cx,
                &prepare(
                    "MATCH (n:Item)-[e:R]->(m) SET n:Seen \
                     RETURN DISTINCT n.p AS p, n.q AS q, m.p AS mp",
                ),
                policy(),
            )
            .unwrap();
        // The first statement zeroed every p and removed q, in this transaction.
        assert_eq!(result.value, rows(vec![vec![int(0), null(), int(0)]]));
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (n:Seen) RETURN n.p AS p, n.q AS q"),
            vec![vec![int(0), null()]; 3]
        );
    });
}

/// A failing statement stages nothing and returns no row: a conflicting
/// pair of assignments, and a RETURN read of a vertex the statement deletes.
#[test]
fn a_refused_statement_stages_nothing() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        // Vertex 4 is `b` in all four rows, which assign it four values.
        let conflict = txn.execute_graph_mutation_query_governed(
            &mut db,
            cx,
            &prepare("MATCH (a:Item), (b:Item {p: 4}) SET b.q = a.p RETURN b.q"),
            policy(),
        );
        assert!(matches!(
            conflict,
            Err(GqlQueryError::Source(GraphMutationQueryError::Mutation(
                GraphMutationError::ConflictingAssignment { .. }
            )))
        ));
        let deleted = txn.execute_graph_mutation_query_governed(
            &mut db,
            cx,
            &prepare("MATCH (n:Item {p: 1}) DETACH DELETE n RETURN n.p"),
            policy(),
        );
        assert!(matches!(
            deleted,
            Err(GqlQueryError::Source(GraphMutationQueryError::Mutation(
                GraphMutationError::DeletedElementRead { row: 0, binding: 0 }
            )))
        ));
        // Identities of deleted vertices are returned.
        let (_, result) = txn
            .execute_graph_mutation_query_governed(
                &mut db,
                cx,
                &prepare("MATCH (n:Item {p: 1}) DETACH DELETE n RETURN n"),
                policy(),
            )
            .unwrap();
        assert_eq!(result.value, rows(vec![vec![GraphValue::Vertex(VId(1))]]));
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(
            read(
                &db,
                cx,
                "MATCH (n:Item) RETURN n.p AS p, n.q AS q ORDER BY p"
            ),
            vec![
                vec![int(2), null()],
                vec![int(3), null()],
                vec![int(4), int(10)],
            ]
        );
    });
}

/// Reads whose post-statement value the statement cannot know are refused
/// at preparation, before any catalog callback runs.
#[test]
fn unknowable_post_statement_reads_refuse_at_preparation() {
    for text in [
        "MATCH (n:Item) SET n:Seen RETURN labels(n)",
        "MATCH (n:Item) DETACH DELETE n RETURN labels(n)",
        "MATCH (a:Item)-[e:R]->(b) DETACH DELETE a RETURN e.w",
        "MATCH (n:Item) SET n.p = 1 RETURN n.p AS x, n.q AS x",
    ] {
        let mut calls = 0;
        let refused = PreparedGraphMutationQueryText::prepare(text, R, |kind, name| {
            calls += 1;
            symbols(kind, name)
        });
        assert!(refused.is_err(), "{text}");
        assert_eq!(calls, 0, "{text}");
    }
    // A mutation without RETURN is not this statement shape.
    assert!(!PreparedGraphMutationQueryText::has_return_clause("MATCH (n) SET n.p = 1").unwrap());
}

fn merge(text: &str) -> PreparedGraphVertexUpsertQuery {
    PreparedGraphVertexUpsertQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}
fn upsert_policy() -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(query_policy()), 100)
}

/// MERGE RETURN is the one chosen vertex after every clause: the matched
/// branch's ON MATCH SET, or the created vertex with its ON CREATE SET and
/// the trailing SET, or the vertex as it is when no clause applies.
#[test]
fn merge_returns_the_chosen_vertex_after_every_clause() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let mut execute = |db: &mut Database<fgdb::MemVfs>, text: &str| {
            txn.execute_graph_vertex_upsert_query_engine_governed(
                db,
                cx,
                &merge(text),
                upsert_policy(),
            )
            .expect(text)
        };
        let (_, outcome, result) = execute(
            &mut db,
            "MERGE (n:Item {p: 4}) ON MATCH SET n.q = n.q + 1 ON CREATE SET n.q = 0 \
             RETURN n, n.p AS p, n.q AS q, n.q * 10 AS ten",
        );
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(4)));
        assert_eq!(
            result.value,
            rows(vec![vec![
                GraphValue::Vertex(VId(4)),
                int(4),
                int(11),
                int(110)
            ]])
        );
        let (_, outcome, result) = execute(
            &mut db,
            "MERGE (n:Item {p: 9}) ON MATCH SET n.q = 100 ON CREATE SET n.q = 1 \
             SET n.w = n.q + 1 RETURN n.p, n.q, n.w",
        );
        assert!(outcome.created());
        assert_eq!(result.value, rows(vec![vec![int(9), int(1), int(2)]]));
        // No clause at all: the vertex as it stands.
        let (_, outcome, result) = execute(&mut db, "MERGE (n:Item {p: 1}) RETURN n.q AS q");
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
        assert_eq!(result.value, rows(vec![vec![null()]]));
        drop(execute);
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(
                &db,
                cx,
                "MATCH (n:Item) WHERE n.p >= 4 RETURN n.p AS p, n.q AS q, n.w AS w ORDER BY p"
            ),
            vec![vec![int(4), int(11), null()], vec![int(9), int(1), int(2)],]
        );
    });
}

/// A RETURN that fails after MERGE created its vertex stages nothing.
#[test]
fn a_failing_merge_return_rolls_the_upsert_back() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let failed = txn.execute_graph_vertex_upsert_query_engine_governed(
            &mut db,
            cx,
            &merge("MERGE (n:Item {p: 7}) RETURN n.p / 0 AS broken"),
            upsert_policy(),
        );
        assert!(matches!(
            failed,
            Err(GqlQueryError::Source(GraphVertexUpsertError::Returning(_)))
        ));
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), before);
        assert!(read(&db, cx, "MATCH (n:Item {p: 7}) RETURN n.p AS p").is_empty());
        for text in [
            "MERGE (n:Item {p: 1}) RETURN labels(n)",
            "MERGE (n:Item {p: 1}) RETURN m.p",
        ] {
            assert!(
                PreparedGraphVertexUpsertQueryText::prepare(text, R, symbols).is_err(),
                "{text}"
            );
        }
    });
}

/// The MATCH row and creation arm's unit input are private. A zero final
/// row allowance must permit LIMIT 0 on either branch, while a visible
/// RETURN row must refuse and undo the statement's complete staged effect.
#[test]
fn merge_return_row_allowance_applies_after_projection_on_both_branches() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let basis = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let mut zero_rows = upsert_policy();
        zero_rows.merge.query.rows = fgdb_gql::GqlExecutionBudget::new(1_000_000, 0);
        let (_, matched, rows) = txn
            .execute_graph_vertex_upsert_query_engine_governed(
                &mut db,
                cx,
                &merge("MERGE (n:Item {p:4}) ON MATCH SET n.q=n.q+1 RETURN n.q AS q LIMIT 0"),
                zero_rows,
            )
            .unwrap();
        assert_eq!(matched, GraphVertexMergeOutcome::Matched(VId(4)));
        assert!(rows.value.is_empty());
        let (_, created, rows) = txn
            .execute_graph_vertex_upsert_query_engine_governed(
                &mut db,
                cx,
                &merge("MERGE (n:Item {p:9}) ON CREATE SET n.q=3 RETURN n.q AS q LIMIT 0"),
                zero_rows,
            )
            .unwrap();
        assert!(created.created());
        assert!(rows.value.is_empty());
        assert_eq!(
            txn.vertex_property(&db, VId(4), Q).unwrap(),
            Some(CanonicalScalar::Int(11)),
        );
        assert_eq!(
            txn.vertex_property(&db, created.vertex(), Q).unwrap(),
            Some(CanonicalScalar::Int(3)),
        );
        let digest = txn.staged_effect_digest().unwrap();
        for statement in [
            "MERGE (n:Item {p:4}) ON MATCH SET n.q=99 RETURN n.q AS q",
            "MERGE (n:Item {p:10}) ON CREATE SET n.q=99 RETURN n.q AS q",
        ] {
            let error = txn
                .execute_graph_vertex_upsert_query_engine_governed(
                    &mut db,
                    cx,
                    &merge(statement),
                    zero_rows,
                )
                .unwrap_err();
            assert!(
                matches!(error, GqlQueryError::Rows(error)
                    if error.dimension == fgdb_gql::GqlBudgetDimension::ResultRows),
                "{statement}",
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert_eq!(db.frontier().unwrap(), basis);
        }
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(
                &db,
                cx,
                "MATCH (n:Item) WHERE n.p>=4 RETURN n.p AS p,n.q AS q ORDER BY p",
            ),
            vec![vec![int(4), int(11)], vec![int(9), int(3)]],
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}


/// Aggregation consumes the statement's complete post-effect occurrence bag.
/// Refused aggregate arguments or output conversions restore that statement's
/// overlay while retaining an earlier successful write in the same transaction.
#[test]
fn aggregate_return_uses_post_statement_values_and_refusals_restore_the_overlay() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let basis = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let mut one = policy();
        one.query.rows = fgdb_gql::GqlExecutionBudget::new(1_000_000, 1);
        let (stats, returned) = txn
            .execute_graph_mutation_query_governed(
                &mut db,
                cx,
                &prepare(
                    "MATCH (n:Item) SET n.p=n.p+10 \
                     RETURN count(*) AS rows,sum(n.p) AS total,max(n.p) AS largest",
                ),
                one,
            )
            .unwrap();
        assert_eq!(stats.effects, 4);
        assert_eq!(returned.value, rows(vec![vec![int(4), int(50), int(14)]]));
        assert_eq!(returned.rows.result_rows, 1);
        assert_eq!(db.frontier().unwrap(), basis);
        let staged = txn.staged_effect_digest().unwrap();
        let mut hidden = one;
        hidden.query.rows = fgdb_gql::GqlExecutionBudget::new(1_000_000, 0);

        for text in [
            "MATCH (n:Item) SET n.q=88 RETURN sum(n.p/(n.p-13)) AS total LIMIT 0",
            "MATCH (n:Item) SET n.q=88 RETURN sum(9223372036854775807) AS wide LIMIT 0",
            "MATCH (n:Item) DETACH DELETE n RETURN sum(n.p) AS deleted LIMIT 0",
        ] {
            assert!(
                txn.execute_graph_mutation_query_governed(
                    &mut db, cx, &prepare(text), hidden,
                ).is_err(),
                "{text}",
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), staged);
            assert_eq!(db.frontier().unwrap(), basis);
            assert_eq!(txn.vertex_property(&db, VId(1), P).unwrap(), Some(CanonicalScalar::Int(11)));
            assert_eq!(txn.vertex_property(&db, VId(4), Q).unwrap(), Some(CanonicalScalar::Int(10)));
        }

        let (_, returned) = txn
            .execute_graph_mutation_query_governed(
                &mut db,
                cx,
                &prepare("MATCH (n:Item) SET n.q=0 RETURN count(*) AS rows LIMIT 0"),
                hidden,
            )
            .unwrap();
        assert!(returned.value.is_empty());
        for id in 1..=4 {
            assert_eq!(txn.vertex_property(&db, VId(id), Q).unwrap(), Some(CanonicalScalar::Int(0)));
        }
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (n:Item) RETURN n.p AS p,n.q AS q ORDER BY p"),
            vec![
                vec![int(11), int(0)],
                vec![int(12), int(0)],
                vec![int(13), int(0)],
                vec![int(14), int(0)],
            ],
        );
    });
}
