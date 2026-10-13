//! Grouped mutation continuations over the real embedded transaction owner.
//! Expected values follow the hand-enumerated three-vertex fixture, including
//! duplicate selection occurrences, simultaneous assignments and final fields.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationError, GraphMutationPolicy,
    GraphMutationQueryError, GraphSymbol, GraphSymbolKind, PreparedGraphMutationQuery,
    PreparedGraphMutationQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, QueryCx, TxnCx, VId,
};

const R: RelationId = RelationId(1);
const ITEM: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const W: PropertyKeyId = PropertyKeyId(3);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
        _ => None,
    }
}

fn query_policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, rows, 100_000_000, 10_000_000)
}

fn policy(rows: u64) -> GraphMutationPolicy {
    GraphMutationPolicy::new(query_policy(rows), 10_000)
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

fn graph() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for id in 1..=3_i64 {
        batch.create_vertex(
            VId(id as u128),
            vec![ITEM],
            vec![
                (P, CanonicalScalar::Int(id)),
                (Q, CanonicalScalar::Int(id * 10)),
            ],
        );
    }
    for (id, destination, weight) in [(11, 2, 5), (12, 3, 7)] {
        batch.add_edge(
            EId(id),
            VId(1),
            VId(destination),
            vec![(W, CanonicalScalar::Int(weight))],
        );
    }
    batch
}

fn run<T>(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx) -> T) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let query = contexts.query();
    let txn = contexts.txn();
    runtime.block_on(test(&commit, &query, &txn))
}

fn read(db: &Database<fgdb::MemVfs>, cx: &QueryCx, text: &str) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = db
        .query(cx, text, &GqlParameters::new(), symbols, query_policy(100))
        .expect(text)
    else {
        panic!("expected read rows");
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => panic!("expected ordinary value cells"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn grouped_vertex_updates_read_final_fields_and_count_selection_occurrences() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let query = prepare(
            "MATCH (a:Item),(b:Item) SET a.p=a.p+10, a.q=a.q+100 \
             WITH count(*) AS occurrences, a AS node, sum(a.p) AS total \
             RETURN node.p AS p, node.q AS q, occurrences, total ORDER BY p",
        );
        let (_, result) = txn
            .execute_graph_mutation_query_governed(&mut db, cx, &query, policy(3))
            .unwrap();
        // Nine matches, three per updated identity. All repeated assignments
        // agree; sums and hidden reads both see the final 11/12/13 values.
        assert_eq!(
            result.value,
            rows(vec![
                vec![int(11), int(110), int(3), int(33)],
                vec![int(12), int(120), int(3), int(36)],
                vec![int(13), int(130), int(3), int(39)],
            ])
        );
        assert_eq!(result.rows.result_rows, 3);
        assert_eq!(db.frontier().unwrap(), before);
        assert!(matches!(
            txn.finish(&mut db, commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(
            read(
                &db,
                cx,
                "MATCH (n:Item) RETURN n.p AS p, n.q AS q ORDER BY p"
            ),
            vec![
                vec![int(11), int(110)],
                vec![int(12), int(120)],
                vec![int(13), int(130)]
            ]
        );
    });
}

#[test]
fn grouped_relationship_updates_keep_parallel_occurrences_without_reapplying_set() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let query = prepare(
            "MATCH (a:Item)-[r:R]->(b:Item),(x:Item) SET r.w=r.w+1 \
             WITH count(*) AS n, r AS edge RETURN edge.w AS w, n ORDER BY w",
        );
        let (_, result) = txn
            .execute_graph_mutation_query_governed(&mut db, cx, &query, policy(2))
            .unwrap();
        assert_eq!(
            result.value,
            rows(vec![vec![int(6), int(3)], vec![int(8), int(3)]])
        );
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r.w AS w ORDER BY w"),
            vec![vec![int(6)], vec![int(8)]]
        );
    });
}

#[test]
fn removed_grouped_properties_are_null_and_limit_zero_does_not_limit_writes() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let (_, result) = txn
            .execute_graph_mutation_query_governed(
                &mut db,
                cx,
                &prepare(
                    "MATCH (n:Item) REMOVE n.q WITH n AS node, count(*) AS c \
                     WHERE node.q IS NULL RETURN node.p AS p, node.q AS q, c ORDER BY p",
                ),
                policy(3),
            )
            .unwrap();
        assert_eq!(
            result.value,
            rows(vec![
                vec![int(1), null(), int(1)],
                vec![int(2), null(), int(1)],
                vec![int(3), null(), int(1)],
            ])
        );
        let (_, result) = txn
            .execute_graph_mutation_query_governed(
                &mut db,
                cx,
                &prepare(
                    "MATCH (n:Item) SET n.q=1 WITH n AS node, count(*) AS c \
                     RETURN node.q AS q LIMIT 0",
                ),
                policy(0),
            )
            .unwrap();
        assert!(result.value.is_empty());
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (n:Item) RETURN n.q AS q"),
            vec![vec![int(1)]; 3]
        );
    });
}

#[test]
fn grouped_head_window_and_unwind_shape_results_not_the_mutation_selection() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let (_, result) = txn
            .execute_graph_mutation_query_governed(
                &mut db,
                cx,
                &prepare(
                    "MATCH (n:Item) SET n.p=n.p+10 \
                     WITH n AS node, n.p AS rank, count(*) AS c ORDER BY rank LIMIT 1 \
                     UNWIND [node.p,c] AS x RETURN sum(x) AS total",
                ),
                policy(1),
            )
            .unwrap();
        assert_eq!(result.value, rows(vec![vec![int(12)]]));
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (n:Item) RETURN n.p AS p ORDER BY p"),
            vec![vec![int(11)], vec![int(12)], vec![int(13)]]
        );
    });
}

#[test]
fn failed_grouped_results_roll_back_the_statement_but_keep_prior_staging() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        txn.execute_graph_mutation_query_governed(
            &mut db,
            cx,
            &prepare("MATCH (n:Item {p:1}) SET n.q=99 RETURN n.q"),
            policy(1),
        )
        .unwrap();
        let failed = txn.execute_graph_mutation_query_governed(
            &mut db,
            cx,
            &prepare(
                "MATCH (n:Item) SET n.p=0 WITH n AS node, count(*) AS c \
                 RETURN 12/node.p AS result LIMIT 0",
            ),
            policy(0),
        );
        assert!(matches!(failed, Err(GqlQueryError::Source(_))));
        let deleted = txn.execute_graph_mutation_query_governed(
            &mut db,
            cx,
            &prepare(
                "MATCH (n:Item {p:1}) DETACH DELETE n \
                 WITH n AS node, count(*) AS c RETURN node.p AS p",
            ),
            policy(1),
        );
        assert!(matches!(
            deleted,
            Err(GqlQueryError::Source(GraphMutationQueryError::Mutation(
                GraphMutationError::DeletedElementRead { .. }
            )))
        ));
        let budget = txn.execute_graph_mutation_query_governed(
            &mut db,
            cx,
            &prepare("MATCH (n:Item) SET n.p=0 WITH n AS node, count(*) AS c RETURN node.p AS p"),
            policy(0),
        );
        assert!(budget.is_err());
        assert_eq!(db.frontier().unwrap(), before);
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(
                &db,
                cx,
                "MATCH (n:Item) RETURN n.p AS p, n.q AS q ORDER BY p"
            ),
            vec![
                vec![int(1), int(99)],
                vec![int(2), int(20)],
                vec![int(3), int(30)]
            ]
        );
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r.w AS w ORDER BY w"),
            vec![vec![int(5)], vec![int(7)]]
        );
    });
}

#[test]
fn optional_null_identity_has_one_occurrence_while_required_empty_has_none() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let mut txn = db.begin(txcx).unwrap();
        for (prefix, expected, grant) in [
            ("OPTIONAL MATCH", vec![vec![null(), int(1)]], 1),
            ("MATCH", Vec::new(), 0),
        ] {
            let text = format!(
                "{prefix} (n:Item {{p:99}}) SET n.q=1 \
                 WITH n AS node, count(*) AS c RETURN node.q AS q, c"
            );
            let (_, result) = txn
                .execute_graph_mutation_query_governed(&mut db, cx, &prepare(&text), policy(grant))
                .unwrap();
            assert_eq!(result.value, rows(expected));
        }
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (n:Item) RETURN n.q AS q ORDER BY q"),
            vec![vec![int(10)], vec![int(20)], vec![int(30)]]
        );
    });
}

#[test]
fn rebound_grouped_filters_may_hide_results_without_discarding_the_writes() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let text = "MATCH (n:Item) SET n.p=$value \
            WITH n AS node, count(*) AS c WHERE node.p >= $minimum \
            RETURN node.p AS p, c";
        let template = PreparedGraphMutationQueryText::prepare(text, R, symbols).unwrap();
        assert_eq!(template.parameter_schema().len(), 2);
        for (value, minimum, expected, grant) in [
            (4, 4, vec![vec![int(4), int(1)]; 3], 3),
            (8, 9, Vec::new(), 0),
        ] {
            let arguments = GqlParameters::new()
                .with_int64("value", value)
                .unwrap()
                .with_int64("minimum", minimum)
                .unwrap();
            let query = template.bind_parameters(&arguments).unwrap();
            let (_, result) = txn
                .execute_graph_mutation_query_governed(&mut db, cx, &query, policy(grant))
                .unwrap();
            assert_eq!(result.value, rows(expected));
        }
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (n:Item) RETURN n.p AS p"),
            vec![vec![int(8)]; 3]
        );
    });
}
