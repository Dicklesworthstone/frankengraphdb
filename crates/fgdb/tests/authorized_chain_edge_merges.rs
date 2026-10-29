//! Native vertex and relationship MERGE chains share scope, work and one completion.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertStats};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphWriteChain,
    PreparedGraphWriteChainText,
};
use fgdb_types::{
    CanonicalF64, CanonicalPropertyValue, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId,
    EId, EmbeddedTxnCompletion, PurposeContexts, QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Restriction, Rights, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x61; 32]);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const Q: PropertyKeyId = PropertyKeyId(3);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x60; 32], NS, [0x62; 32])
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9167), NS, "graph", SchemaEpoch(1), 1).unwrap()
}

fn grant(rights: Rights, max_rows: u64) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R, S]),
        properties: Scope::only([P, Q]),
        rights,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows,
        },
        expires_at_ms: 10_000,
    }
}

fn policy(rows: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(10_000, rows, 1_000_000, 100_000),
        100,
        100,
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn chain(text: &str) -> PreparedGraphWriteChain {
    chain_with(text, &GqlParameters::new())
}

fn chain_with(text: &str, parameters: &GqlParameters) -> PreparedGraphWriteChain {
    assert!(
        PreparedGraphWriteChainText::is_chain(text).unwrap(),
        "{text}"
    );
    let template = PreparedGraphWriteChainText::prepare(text, R, symbols).expect(text);
    let result = template.bind_parameters(parameters).expect(text);
    assert_eq!(template.returns_rows(), result.returns_rows());
    result
}

fn authorization(error: &(dyn core::error::Error + 'static)) -> Option<Error> {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        cause = error.source();
    }
    None
}

fn lab<T, F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let (result, report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
    result
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

async fn whole_pattern_merge(
    contexts: &PurposeContexts,
    db: &mut Database<MemVfs>,
    authorized: bool,
    program: &PreparedGraphWriteChain,
    allowance: GraphInsertPolicy,
) -> Result<(GraphInsertStats, Vec<GraphValueRow>), String> {
    let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
    let result = if authorized {
        let issuer = authority();
        let token = issuer
            .issue_at(&grant(Rights::ReadWrite, 100_000), NOW)
            .unwrap();
        db.execute_graph_write_chain_authorized(
            &txn,
            &query,
            &commit,
            &issuer,
            &token,
            "main",
            program,
            allowance,
            || NOW,
        )
        .await
        .map(|(stats, rows, _)| (stats, rows.value))
        .map_err(|error| format!("{error:?}"))
    } else {
        let mut transaction = db.begin(&txn).unwrap();
        match transaction.execute_graph_write_chain_engine_governed(db, &query, program, allowance)
        {
            Ok((stats, rows, _)) => {
                transaction.finish(db, &commit).await.unwrap();
                Ok((stats, rows.value))
            }
            Err(error) => Err(format!("{error:?}")),
        }
    };
    assert_eq!(txn.outstanding_obligations(), 0);
    result
}

#[test]
fn whole_pattern_merge_creates_missing_endpoints_without_reusing_partial_matches() {
    lab(0xc4a1_3810, |contexts| async move {
        let commit = contexts.commit();
        for authorized in [false, true] {
            for (text, created, reverse, same) in [
                (
                    "MATCH (a:Visible {p:1}) MERGE (a)-[r:R {q:7}]->(b:Visible {p:2}) RETURN a,b,r",
                    1,
                    false,
                    false,
                ),
                (
                    "MERGE (a:Visible {p:1})-[r:R {q:7}]->(b:Visible {p:2}) RETURN a,b,r",
                    2,
                    false,
                    false,
                ),
                (
                    "MERGE (a:Visible {p:1})<-[r:R {q:7}]-(b:Visible {p:2}) RETURN a,b,r",
                    2,
                    true,
                    false,
                ),
                (
                    "MERGE (a:Visible {p:1})-[r:R {q:7}]-(b:Visible {p:2}) RETURN a,b,r",
                    2,
                    false,
                    false,
                ),
                (
                    "MERGE (a:Visible {p:1})-[r:R {q:7}]->(a) RETURN a,a AS b,r",
                    1,
                    false,
                    true,
                ),
            ] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(S);
                for id in 1..=2 {
                    seed.create_vertex(
                        VId(id as u128),
                        vec![L],
                        vec![(P, CanonicalScalar::Int(id))],
                    );
                }
                db.write(&commit, seed).await.unwrap();
                let program = chain(text);
                let (stats, rows) =
                    whole_pattern_merge(&contexts, &mut db, authorized, &program, policy(10))
                        .await
                        .unwrap_or_else(|error| panic!("{text}: {error}"));
                assert_eq!(
                    (stats.created_vertices, stats.created_edges),
                    (created, 1),
                    "{text}"
                );
                assert_eq!(rows.len(), 1, "{text}");
                let [
                    GraphValue::Vertex(a),
                    GraphValue::Vertex(b),
                    GraphValue::Edge(r),
                ] = rows[0].values()
                else {
                    panic!("expected both resolved endpoints and relationship: {text}");
                };
                let (a, b, r) = (*a, *b, *r);
                if text.starts_with("MATCH") {
                    assert_eq!(a, VId(1));
                } else {
                    assert_ne!(
                        a,
                        VId(1),
                        "a matching isolated node is not a complete pattern"
                    );
                    assert_ne!(a, VId(2));
                }
                assert_ne!(
                    b,
                    VId(2),
                    "the isolated matching destination must survive unused"
                );
                assert_eq!(a == b, same, "{text}");
                let edge = db
                    .edges()
                    .unwrap()
                    .into_iter()
                    .find(|edge| edge.entry.eid == r)
                    .unwrap();
                assert_eq!(
                    (edge.entry.src, edge.entry.dst),
                    if reverse { (b, a) } else { (a, b) }
                );
                assert_eq!(edge.props, [(Q, CanonicalScalar::Int(7).into())]);
                assert_eq!(db.vertices().unwrap().len(), 2 + created as usize);
                assert_eq!(
                    db.vertex(VId(1)).unwrap().unwrap().props,
                    [(P, CanonicalScalar::Int(1).into())]
                );
                assert_eq!(
                    db.vertex(VId(2)).unwrap().unwrap().props,
                    [(P, CanonicalScalar::Int(2).into())]
                );
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let mut matched_only = policy(10);
                matched_only.max_vertices = 0;
                matched_only.max_edges = 0;
                let (again, repeated) =
                    whole_pattern_merge(&contexts, &mut db, authorized, &program, matched_only)
                        .await
                        .unwrap_or_else(|error| panic!("repeat {text}: {error}"));
                assert_eq!((again.created_vertices, again.created_edges), (0, 0));
                assert_eq!(repeated, rows);
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
            }
        }
    });
}

#[test]
fn whole_pattern_merge_anonymous_endpoints_stay_distinct_and_private_across_steps() {
    lab(0xc4a1_3817, |contexts| async move {
        let commit = contexts.commit();
        for authorized in [false, true] {
            for (text, expected_columns, vertices) in [
                (
                    "MERGE (a:Visible {p:1})-[:R]->(:Visible {p:2}) \
                     MERGE (a)-[:S]->(:Visible {p:3}) RETURN a",
                    vec!["a"],
                    3,
                ),
                (
                    "MERGE (a:Visible {p:1})-[:R]->(:Visible {p:2}) \
                     MERGE (a)-[:S]->(:Visible {p:3}) \
                     CREATE (c:Visible {p:4}) RETURN *",
                    vec!["a", "c"],
                    4,
                ),
            ] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let program = chain(text);
                assert_eq!(program.columns(), expected_columns, "{text}");
                let (stats, rows) =
                    whole_pattern_merge(&contexts, &mut db, authorized, &program, policy(10))
                        .await
                        .unwrap_or_else(|error| panic!("{text}: {error}"));
                assert_eq!((stats.created_vertices, stats.created_edges), (vertices, 2));
                assert_eq!(rows.len(), 1, "{text}");
                assert_eq!(rows[0].values().len(), expected_columns.len());
                let GraphValue::Vertex(a) = rows[0].values()[0] else {
                    panic!("the named source is the first public result");
                };
                let edges = db.edges().unwrap();
                let r = edges.iter().find(|edge| edge.entry.relation == R).unwrap();
                let s = edges.iter().find(|edge| edge.entry.relation == S).unwrap();
                assert_eq!((r.entry.src, s.entry.src), (a, a));
                assert_ne!(r.entry.dst, s.entry.dst);
                assert_ne!(r.entry.dst, a);
                assert_ne!(s.entry.dst, a);
                for (id, value) in [(a, 1), (r.entry.dst, 2), (s.entry.dst, 3)] {
                    assert_eq!(
                        db.vertex(id).unwrap().unwrap().props,
                        [(P, CanonicalScalar::Int(value).into())]
                    );
                }
                assert_eq!(db.vertices().unwrap().len(), vertices as usize);
                if expected_columns.len() == 2 {
                    let GraphValue::Vertex(c) = rows[0].values()[1] else {
                        panic!("only the named CREATE vertex follows the named MERGE source");
                    };
                    assert_eq!(
                        db.vertex(c).unwrap().unwrap().props,
                        [(P, CanonicalScalar::Int(4).into())]
                    );
                }
            }
        }
    });
}

#[test]
fn whole_pattern_merge_reuses_prior_occurrences_and_runs_new_endpoint_actions() {
    lab(0xc4a1_3811, |contexts| async move {
        let commit = contexts.commit();
        for authorized in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(S);
            seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(1))]);
            db.write(&commit, seed).await.unwrap();
            let program = chain(
                "MATCH (a:Visible {p:1}) UNWIND [1,1] AS occurrence \
                 MERGE (a)-[r:R {p:7}]->(b:Visible {p:2}) \
                 ON CREATE SET b.q=10,r.q=1 \
                 ON MATCH SET b.q=b.q+1,r.q=b.q SET r.q=r.q+1 \
                 RETURN b.p AS key,b.q AS vertex_value,r.q AS edge_value",
            );
            let (stats, rows) = whole_pattern_merge(
                &contexts,
                &mut db,
                authorized,
                &program,
                policy(10).with_mutation_limit(10),
            )
            .await
            .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (1, 1));
            assert_eq!(db.vertices().unwrap().len(), 2);
            assert_eq!(db.edges().unwrap().len(), 1);
            // First occurrence creates q=10 and edge q=1, then SET makes 2.
            // Second matches that full pattern: its simultaneous ON MATCH
            // reads old b.q=10 for both assignments, then SET makes edge q=11.
            assert_eq!(
                rows,
                vec![GraphValueRow::from_owned_values(vec![int(2), int(11), int(11)]); 2]
            );

            let program = chain(
                "MATCH (a:Visible {p:1}) WITH a UNWIND [{p:3,q:9},{p:3,q:8}] AS row \
                 MERGE (a)-[r:S]->(b:Visible {p:row.p}) \
                 ON CREATE SET b.q=row.q ON MATCH SET b.q=row.q RETURN b.p AS key,b.q AS value",
            );
            let (stats, rows) =
                whole_pattern_merge(&contexts, &mut db, authorized, &program, policy(10))
                    .await
                    .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (1, 1));
            assert_eq!(
                rows,
                vec![GraphValueRow::from_owned_values(vec![int(3), int(8)]); 2]
            );
        }
    });
}

#[test]
fn whole_pattern_merge_keeps_parallel_matches_and_live_binding_provenance() {
    lab(0xc4a1_3812, |contexts| async move {
        let commit = contexts.commit();
        for authorized in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(
                VId(1),
                vec![L],
                vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(3))],
            );
            seed.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(2))]);
            for (edge, q) in [(10, 10), (11, 20)] {
                seed.add_edge(
                    EId(edge),
                    VId(1),
                    VId(2),
                    vec![(Q, CanonicalScalar::Int(q))],
                );
            }
            seed.add_edge(EId(12), VId(1), VId(1), vec![]);
            db.write(&commit, seed).await.unwrap();
            let program = chain(
                "MATCH (a:Visible {p:1}) UNWIND [0,0] AS occurrence \
                 MERGE (a)-[r:R]->(b:Visible {p:2}) ON MATCH SET r.q=r.q+1 \
                 RETURN b,r,r.q AS value ORDER BY r",
            );
            let mut bounded = policy(10).with_mutation_limit(10);
            bounded.max_vertices = 0;
            bounded.max_edges = 0;
            let (stats, rows) =
                whole_pattern_merge(&contexts, &mut db, authorized, &program, bounded)
                    .await
                    .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
            assert_eq!(
                rows,
                [(10, 12), (10, 12), (11, 22), (11, 22)]
                    .map(|(edge, q)| {
                        GraphValueRow::from_owned_values(vec![
                            GraphValue::Vertex(VId(2)),
                            GraphValue::Edge(EId(edge)),
                            int(q),
                        ])
                    })
                    .to_vec()
            );

            // Newly bound b and carried a name the SAME physical vertex.
            // Provenance chooses the cut: b sees earlier occurrences, while
            // a remains the frozen input. Identity-based dispatch is wrong.
            let program = chain(
                "MATCH (a:Visible {p:1}) WITH a UNWIND [0,0] AS occurrence \
                 MERGE (a)-[r:R]->(b:Visible {p:1}) \
                 ON MATCH SET b.q=b.q+1,r.q=a.q RETURN b.q AS live,r.q AS frozen",
            );
            let (stats, rows) =
                whole_pattern_merge(&contexts, &mut db, authorized, &program, bounded)
                    .await
                    .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
            assert_eq!(
                rows,
                vec![GraphValueRow::from_owned_values(vec![int(5), int(3)]); 2]
            );
        }
    });
}

#[test]
fn whole_pattern_merge_reads_only_reached_new_endpoint_action_operands() {
    lab(0xc4a1_3816, |contexts| async move {
        let commit = contexts.commit();
        let program = chain(
            "MATCH (a:Visible {p:1}) UNWIND [0,0] AS occurrence \
             MERGE (a)-[r:R]->(b:Visible {p:1}) \
             ON MATCH SET b.q=CASE WHEN occurrence=0 THEN b.q+1 ELSE size(b.secret) END",
        );
        for authorized in [false, true] {
            let mut baseline = None;
            for size in [8, 16_384] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(R);
                seed.create_vertex(
                    VId(1),
                    vec![L],
                    vec![
                        (P, CanonicalScalar::Int(1)),
                        (SECRET, CanonicalScalar::bytes(vec![0x57; size]).unwrap()),
                        (Q, CanonicalScalar::Int(3)),
                    ],
                );
                seed.add_edge(EId(1), VId(1), VId(1), vec![]);
                db.write(&commit, seed).await.unwrap();
                let (stats, rows) = whole_pattern_merge(
                    &contexts,
                    &mut db,
                    authorized,
                    &program,
                    policy(0).with_mutation_limit(2),
                )
                .await
                .unwrap();
                assert!(rows.is_empty());
                assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
                assert_eq!(
                    db.vertex(VId(1))
                        .unwrap()
                        .unwrap()
                        .props
                        .iter()
                        .find(|(key, _)| *key == Q)
                        .map(|(_, value)| value.clone()),
                    Some(CanonicalScalar::Int(5).into())
                );
                if let Some(expected) = baseline {
                    assert_eq!(
                        stats, expected,
                        "an unselected CASE operand cannot read or copy its payload"
                    );
                } else {
                    baseline = Some(stats);
                }
            }
        }
    });
}

#[test]
fn whole_pattern_merge_rolls_back_created_endpoints_on_late_value_or_limit_refusal() {
    lab(0xc4a1_3813, |contexts| async move {
        let commit = contexts.commit();
        for authorized in [false, true] {
            for (text, vertices, edges) in [
                (
                    "MATCH (a:Visible {p:1}) UNWIND [{p:2},{p:null}] AS row MERGE (a)-[:R]->(b:Visible {p:row.p})",
                    100,
                    100,
                ),
                (
                    "MATCH (a:Visible {p:1}) UNWIND [{p:2},{p:{nested:3}}] AS row MERGE (a)-[:R]->(b:Visible {p:row.p})",
                    100,
                    100,
                ),
                (
                    "MATCH (a:Visible {p:1}) UNWIND [1,2] AS x MERGE (a)-[:R]->(b:Visible {p:x+1}) ON CREATE SET b.q=7/(2-x)",
                    100,
                    100,
                ),
                (
                    "MATCH (a:Visible {p:1}) UNWIND [2,3] AS x MERGE (a)-[:R]->(b:Visible {p:x})",
                    1,
                    100,
                ),
                (
                    "MATCH (a:Visible {p:1}) UNWIND [2,3] AS x MERGE (a)-[:R]->(b:Visible {p:x})",
                    100,
                    1,
                ),
                (
                    "MATCH (a:Visible {p:1}) WITH a,[a,7] AS targets UNWIND targets AS target \
                     MERGE (target)-[:R]->(b:Visible {p:2})",
                    100,
                    100,
                ),
            ] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(S);
                seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(1))]);
                db.write(&commit, seed).await.unwrap();
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let mut allowance = policy(10);
                allowance.max_vertices = vertices;
                allowance.max_edges = edges;
                assert!(
                    whole_pattern_merge(&contexts, &mut db, authorized, &chain(text), allowance)
                        .await
                        .is_err(),
                    "{text}"
                );
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before,
                    "{text}"
                );
            }
        }
    });
}

#[test]
fn whole_pattern_merge_accounts_for_new_vertices_and_shared_native_allowances() {
    async fn attempt(
        contexts: &PurposeContexts,
        authorized: bool,
        allowance: GraphInsertPolicy,
    ) -> Option<GraphInsertStats> {
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(S);
        seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(1))]);
        db.write(&commit, seed).await.unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let program = chain(
            "MATCH (a:Visible {p:1}) UNWIND [2,3] AS value \
             MERGE (a)-[r:R]->(b:Visible {p:value}) ON CREATE SET b.q=value+10,r.q=value",
        );
        match whole_pattern_merge(contexts, &mut db, authorized, &program, allowance).await {
            Ok((stats, rows)) => {
                assert!(rows.is_empty());
                assert_eq!((stats.created_vertices, stats.created_edges), (2, 2));
                assert_eq!(db.vertices().unwrap().len(), 3);
                assert_eq!(db.edges().unwrap().len(), 2);
                Some(stats)
            }
            Err(_) => {
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
                None
            }
        }
    }
    lab(0xc4a1_3815, |contexts| async move {
        for authorized in [false, true] {
            let allowance = policy(0).with_mutation_limit(4);
            let measured = attempt(&contexts, authorized, allowance).await.unwrap();
            let mut exact = allowance;
            exact.max_vertices = 2;
            exact.max_edges = 2;
            exact.query.evaluator.max_work_units = measured.evaluator.work_units;
            exact.query.evaluator.max_scratch_entries = measured.evaluator.scratch_entries;
            assert_eq!(attempt(&contexts, authorized, exact).await, Some(measured));
            for dimension in 0..5 {
                let mut short = exact;
                match dimension {
                    0 => short.max_vertices -= 1,
                    1 => short.max_edges -= 1,
                    2 => short.max_mutation_effects -= 1,
                    3 => short.query.evaluator.max_work_units -= 1,
                    4 => short.query.evaluator.max_scratch_entries -= 1,
                    _ => unreachable!(),
                }
                assert!(
                    attempt(&contexts, authorized, short).await.is_none(),
                    "dimension {dimension}"
                );
            }
        }
    });
}

#[test]
fn whole_pattern_merge_authorization_hides_candidates_and_refuses_denied_creation() {
    lab(0xc4a1_3814, |contexts| async move {
        let commit = contexts.commit();
        let program = chain(
            "MATCH (a:Visible {p:1}) MERGE (a)-[r:R]->(b:Visible {p:2}) \
             ON CREATE SET b.q=7 RETURN b.p AS key,b.q AS value",
        );
        let mut baseline = None;
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(1))]);
            if hidden {
                for id in 2..=5 {
                    seed.create_vertex(VId(id), vec![HIDDEN], vec![(P, CanonicalScalar::Int(2))]);
                    seed.add_edge(EId(id), VId(1), VId(id), vec![]);
                }
            }
            db.write(&commit, seed).await.unwrap();
            let result = whole_pattern_merge(&contexts, &mut db, true, &program, policy(10))
                .await
                .unwrap();
            assert_eq!((result.0.created_vertices, result.0.created_edges), (1, 1));
            assert_eq!(
                result.1,
                vec![GraphValueRow::from_owned_values(vec![int(2), int(7)])]
            );
            if let Some(expected) = &baseline {
                assert_eq!(
                    &result, expected,
                    "hidden endpoint candidates cannot change visible rows or charged work"
                );
            } else {
                baseline = Some(result);
            }
            for text in [
                "CREATE (marker:Visible) MERGE (a:Visible {p:10})-[:R]->(b:Hidden {p:11})",
                "CREATE (marker:Visible) MERGE (a:Visible {p:10})-[:R]->(b:Visible {secret:11})",
                "CREATE (marker:Visible) MERGE (a:Visible {p:10})-[:R]->(b:Visible {p:11}) ON CREATE SET b.secret=9",
            ] {
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let error = whole_pattern_merge(&contexts, &mut db, true, &chain(text), policy(10))
                    .await
                    .unwrap_err();
                assert!(error.contains("ScopeDenied"), "{text}: {error}");
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before,
                    "{text}"
                );
            }
        }
    });
}

/// A named relationship MERGE path with ON actions whose WITH tail reads to
/// the statement's end, with no further write, refuses before any catalog or
/// execution: the chain hosts run no read tail for a relationship MERGE step
/// (fgdb-at06q; vertex MERGE steps gained one in 60d60df1). The positive law
/// this replaces, action results and authorized masking of `edge.secret` in
/// both hosts, is recorded on fgdb-at06q for when the tail is built.
#[test]
fn named_relationship_path_read_tails_refuse_until_the_chain_runs_them() {
    let text = "MATCH (a:Visible {p:1}),(b:Visible {p:2}) MERGE route=(a)<-[r:R]-(b) \
                ON CREATE SET r.q=7 ON MATCH SET r.q=r.q+1 \
                WITH route AS saved UNWIND relationships(saved) AS edge \
                RETURN saved,edge.q AS value,edge.secret AS masked,length(saved) AS hops";
    assert!(PreparedGraphWriteChainText::is_chain(text).unwrap());
    let refused = PreparedGraphWriteChainText::prepare(text, R, symbols).unwrap_err();
    assert!(
        matches!(
            refused,
            fgdb_gql::GraphWriteChainTextError::EdgeMerge(fgdb_gql::GraphEdgeMergeTextError {
                offset,
                kind: fgdb_gql::GraphEdgeMergeTextErrorKind::Query(
                    fgdb_gql::GraphPatternTextErrorKind::Expected(
                        "a CREATE or MERGE after the WITH of a MERGE step"
                    )
                ),
            }) if offset == text.len()
        ),
        "{refused:?}"
    );
}

#[test]
fn implicit_named_merge_path_rows_feed_the_next_write_in_both_hosts() {
    lab(0xc4a1_3191, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain(
            "MATCH (a:Visible {p:1}),(b:Visible {p:2}) MERGE route=(a)-[:R]->(b) \
             CREATE (marker:Visible {p:length(route)}) RETURN route,marker.p AS hops",
        );
        for authorized in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            for id in 1..=2 {
                seed.create_vertex(
                    VId(id as u128),
                    vec![L],
                    vec![(P, CanonicalScalar::Int(id))],
                );
            }
            db.write(&commit, seed).await.unwrap();
            let before = db.frontier().unwrap();
            let (stats, result) = if authorized {
                let (stats, result, _) = db
                    .execute_graph_write_chain_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &program,
                        policy(1),
                        || NOW,
                    )
                    .await
                    .unwrap();
                (stats, result)
            } else {
                let mut transaction = db.begin(&txn).unwrap();
                let (stats, result, _) = transaction
                    .execute_graph_write_chain_engine_governed(&mut db, &query, &program, policy(1))
                    .unwrap();
                transaction.finish(&mut db, &commit).await.unwrap();
                (stats, result)
            };
            assert_eq!((stats.created_vertices, stats.created_edges), (1, 1));
            let edges = db.edges().unwrap();
            assert_eq!(edges.len(), 1);
            assert_eq!(result.value.len(), 1);
            assert_eq!(
                result.value[0].values(),
                [
                    GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
                        VId(1),
                        Box::new([(edges[0].entry.eid, VId(2))])
                    )),
                    int(1),
                ],
            );
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

/// Runs `text` once in one host over a fresh `(1 {p:1, secret:7})`,
/// `(2 {p:2})` graph: the statement's rows, or its authorization refusal.
async fn carried_merge(
    commit: &CommitCx,
    txn: &TxnCx,
    query: &QueryCx,
    authorized: bool,
    text: &str,
) -> (
    Database<MemVfs>,
    Result<(GraphInsertStats, Vec<GraphValueRow>), Option<Error>>,
) {
    let mut db = Database::open_memory(commit, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(
        VId(1),
        vec![L],
        vec![
            (P, CanonicalScalar::Int(1)),
            (SECRET, CanonicalScalar::Int(7)),
        ],
    );
    seed.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(2))]);
    db.write(commit, seed).await.unwrap();
    let program = chain(text);
    let result = if authorized {
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        db.execute_graph_write_chain_authorized(
            txn,
            query,
            commit,
            &issuer,
            &token,
            "main",
            &program,
            policy(1),
            || NOW,
        )
        .await
        .map(|(stats, result, _)| (stats, result.value))
        .map_err(|error| authorization(&error))
    } else {
        let mut transaction = db.begin(txn).unwrap();
        let (stats, result, _) = transaction
            .execute_graph_write_chain_engine_governed(&mut db, query, &program, policy(1))
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        transaction.finish(&mut db, commit).await.unwrap();
        Ok((stats, result.value))
    };
    assert_eq!(txn.outstanding_obligations(), 0, "{text}");
    (db, result)
}

/// A relationship MERGE input reads a carried vertex it does not rematch
/// (`a.p` of a vertex the statement created or merged) through the host's
/// element reader, with or without ON actions, in both hosts (fgdb-v1rbj).
/// The authorized reader keeps the explicit refusal for a field the grant
/// hides; the unauthorized host reads the same field.
#[test]
fn relationship_merge_inputs_read_carried_vertices_in_both_hosts() {
    lab(0xc4a1_3192, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        for authorized in [false, true] {
            for (text, row, weight) in [
                (
                    "CREATE (a:Visible {p:5}) WITH a MATCH (b:Visible {p:2}) \
                     MERGE (a)<-[r:R]-(b) RETURN a.p AS source,b.p AS target",
                    [int(5), int(2)],
                    None,
                ),
                (
                    "CREATE (a:Visible {p:5}) WITH a MATCH (b:Visible {p:2}) \
                     MERGE (a)<-[r:R]-(b) ON CREATE SET r.q=a.p+b.p \
                     RETURN a.p AS source,r.q AS weight",
                    [int(5), int(7)],
                    Some(7),
                ),
            ] {
                let (db, result) = carried_merge(&commit, &txn, &query, authorized, text).await;
                let (stats, rows) =
                    result.unwrap_or_else(|error| panic!("{authorized} {text}: {error:?}"));
                assert_eq!(
                    (stats.created_vertices, stats.created_edges),
                    (1, 1),
                    "{text}"
                );
                assert_eq!(rows.len(), 1, "{text}");
                assert_eq!(rows[0].values(), row, "{authorized} {text}");
                let created = db
                    .vertices()
                    .unwrap()
                    .into_iter()
                    .find(|vertex| vertex.vid != VId(1) && vertex.vid != VId(2))
                    .unwrap()
                    .vid;
                let edges = db.edges().unwrap();
                assert_eq!(edges.len(), 1, "{text}");
                assert_eq!(
                    (
                        edges[0].entry.src,
                        edges[0].entry.relation,
                        edges[0].entry.dst
                    ),
                    (VId(2), R, created),
                    "{text}"
                );
                let expected: Vec<(PropertyKeyId, CanonicalPropertyValue)> = weight
                    .into_iter()
                    .map(|q| (Q, CanonicalPropertyValue::from(CanonicalScalar::Int(q))))
                    .collect();
                assert_eq!(edges[0].props, expected, "{authorized} {text}");
            }
            let text = "MERGE (a:Visible {p:1}) WITH a MATCH (b:Visible {p:2}) \
                        MERGE (a)-[r:R]->(b) ON CREATE SET r.q=a.secret RETURN r.q AS weight";
            let (db, result) = carried_merge(&commit, &txn, &query, authorized, text).await;
            if authorized {
                assert_eq!(result.err(), Some(Some(Error::ScopeDenied)), "{text}");
                assert!(db.edges().unwrap().is_empty(), "{text}");
                assert_eq!(db.vertices().unwrap().len(), 2, "{text}");
            } else {
                let (stats, rows) = result.unwrap();
                assert_eq!((stats.created_vertices, stats.created_edges), (0, 1));
                assert_eq!(rows.len(), 1, "{text}");
                assert_eq!(rows[0].values(), [int(7)], "{text}");
                let edges = db.edges().unwrap();
                assert_eq!(edges.len(), 1, "{text}");
                assert_eq!(
                    edges[0].props,
                    [(Q, CanonicalPropertyValue::from(CanonicalScalar::Int(7)))]
                );
            }
        }
    });
}

#[test]
fn relationship_action_imports_bind_weights_retry_without_creation_and_reopen() {
    lab(0xc4a1_3114, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let initial = db.frontier().unwrap();
        assert_eq!(initial.0, 0);
        assert!(db.vertices().unwrap().is_empty());
        assert!(db.edges().unwrap().is_empty());
        let parameters = GqlParameters::new()
            .with_list(
                "rows",
                [(1, 2, 5), (2, 3, 7), (3, 3, -2)]
                    .into_iter()
                    .map(|(source, target, weight)| {
                        GraphValue::map(vec![
                            ("s".into(), int(source)),
                            ("d".into(), int(target)),
                            ("w".into(), int(weight)),
                        ])
                        .unwrap()
                    })
                    .collect(),
            )
            .unwrap();
        let original = parameters.canonical_bytes();
        let program = chain_with(
            "UNWIND $rows AS row MERGE (a:Visible {p:row.s}) \
             MERGE (b:Visible {p:row.d}) MERGE (a)-[e:R]->(b) \
             ON CREATE SET e.p=row.w ON MATCH SET e.p=row.w",
            &parameters,
        );
        for attempt in 0..2 {
            let before = db.frontier().unwrap();
            let graph_before = (db.vertices().unwrap(), db.edges().unwrap());
            let mut bounded = policy(0).with_mutation_limit(3);
            if attempt == 1 {
                bounded.max_vertices = 0;
                bounded.max_edges = 0;
            }
            let (stats, result, completion) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(
                (stats.created_vertices, stats.created_edges),
                if attempt == 0 { (3, 3) } else { (0, 0) }
            );
            assert!(result.value.is_empty());
            assert_eq!(result.rows.result_rows, 0);
            let frontier = db.frontier().unwrap();
            assert_eq!(frontier.0, before.0 + 1);
            // Explicit SET intents remain a committed write even when their
            // values normalize to no change (transaction_completion's no-op law).
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: frontier,
                }
            );
            assert_eq!(db.delta_since(before).unwrap().count(), 1);
            if attempt == 1 {
                assert_eq!(
                    (db.vertices().unwrap(), db.edges().unwrap()),
                    graph_before,
                    "retry preserves every identity and property"
                );
            }
        }
        assert_eq!(db.frontier().unwrap().0, initial.0 + 2);
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        let identity = |id| {
            vertices.iter().find(|row| row.vid == id).unwrap().props[0]
                .1
                .clone()
        };
        let mut weighted: Vec<_> = edges
            .iter()
            .map(|edge| {
                assert_eq!(edge.props.len(), 1);
                assert_eq!(edge.props[0].0, P);
                (
                    identity(edge.entry.src),
                    identity(edge.entry.dst),
                    edge.props[0].1.clone(),
                )
            })
            .collect();
        weighted.sort();
        let value = |n| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n));
        assert_eq!(
            weighted,
            vec![
                (value(1), value(2), value(5)),
                (value(2), value(3), value(7)),
                (value(3), value(3), value(-2)),
            ]
        );
        assert_eq!(parameters.canonical_bytes(), original);
        let frontier = db.frontier().unwrap();
        assert_eq!(txn.outstanding_obligations(), 0);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(db.edges().unwrap(), edges);
    });
}

#[test]
fn relationship_actions_visit_each_parallel_match_per_occurrence_before_final_return() {
    lab(0xc4a1_3115, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 4), NOW).unwrap();
        let program = chain(
            "UNWIND [1,2] AS delta MERGE (a:Visible {p:1}) MERGE (b:Visible {p:2}) \
             MERGE (a)-[e:S]->(b) ON CREATE SET e.secret=99 \
             ON MATCH SET e.p=e.p+delta SET e.q=e.p \
             RETURN e AS edge,e.p AS weight,e.q AS after ORDER BY edge",
        );
        for limit in [8, 7] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(S);
            for id in 1..=2 {
                seed.create_vertex(
                    VId(id as u128),
                    vec![L],
                    vec![(P, CanonicalScalar::Int(id))],
                );
            }
            for (id, weight) in [(21, 10), (22, 20)] {
                seed.add_edge(
                    EId(id),
                    VId(1),
                    VId(2),
                    vec![(P, CanonicalScalar::Int(weight))],
                );
            }
            db.write(&commit, seed).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let mut bounded = policy(4).with_mutation_limit(limit);
            bounded.max_vertices = 0;
            bounded.max_edges = 0;
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await;
            if limit == 8 {
                let (stats, result, _) = result.unwrap();
                assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
                assert_eq!(result.rows.result_rows, 4);
                assert_eq!(
                    result
                        .value
                        .iter()
                        .map(GraphValueRow::values)
                        .collect::<Vec<_>>(),
                    vec![
                        &[GraphValue::Edge(EId(21)), int(13), int(13)][..],
                        &[GraphValue::Edge(EId(21)), int(13), int(13)][..],
                        &[GraphValue::Edge(EId(22)), int(23), int(23)][..],
                        &[GraphValue::Edge(EId(22)), int(23), int(23)][..],
                    ],
                    "RETURN reads the final overlay for every retained occurrence"
                );
                assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                assert_eq!(db.edges().unwrap().len(), 2);
            } else {
                let error = result.unwrap_err();
                assert!(format!("{error:?}").contains("EffectLimit"), "{error:?}");
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn relationship_actions_mask_hidden_rhs_and_admit_only_selected_branch_writes() {
    lab(0xc4a1_3116, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 2), NOW).unwrap();
        let mut absent = None;
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(S);
            for id in 1..=2 {
                seed.create_vertex(
                    VId(id as u128),
                    vec![L],
                    vec![(P, CanonicalScalar::Int(id))],
                );
            }
            let mut properties = vec![(P, CanonicalScalar::Int(7))];
            if hidden {
                properties.push((SECRET, CanonicalScalar::Int(99)));
            }
            properties.push((Q, CanonicalScalar::Int(9)));
            seed.add_edge(EId(21), VId(1), VId(2), properties);
            db.write(&commit, seed).await.unwrap();
            let program = chain(
                "UNWIND [1,2] AS x MERGE (a:Visible {p:1}) MERGE (b:Visible {p:2}) \
                 MERGE (a)-[e:S]->(b) ON CREATE SET e.secret=0 \
                 ON MATCH SET e.q=e.secret RETURN e.q AS q",
            );
            let (stats, result, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(2).with_mutation_limit(2),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
            assert_eq!(result.value.len(), 2);
            for row in &result.value {
                assert_eq!(row.values(), &[GraphValue::Scalar(CanonicalScalar::Null)]);
            }
            let edges = db.edges().unwrap();
            assert_eq!(edges.len(), 1);
            assert_eq!(
                edges[0]
                    .props
                    .iter()
                    .find(|(key, _)| *key == Q)
                    .map(|(_, value)| value),
                None,
                "masked NULL removes the old allowed property"
            );
            assert_eq!(edges[0].props.iter().any(|(key, _)| *key == SECRET), hidden);
            let observed = (stats, result.value);
            if let Some(expected) = &absent {
                assert_eq!(
                    &observed, expected,
                    "hidden and absent RHS have identical visible work"
                );
            } else {
                absent = Some(observed);
            }
            let before = (db.frontier().unwrap(), db.vertices().unwrap(), edges);
            for tail in [
                "MERGE (a)-[e:S]->(b) ON MATCH SET e.secret=99",
                "MERGE (a)-[e:R]->(b) ON CREATE SET e.secret=99",
                "MERGE (a)-[e:S]->(b) SET e.secret=99",
            ] {
                let program = chain(&format!(
                    "CREATE (marker:Visible {{p:99}}) MERGE (a:Visible {{p:1}}) \
                     MERGE (b:Visible {{p:2}}) {tail}"
                ));
                let error = db
                    .execute_graph_write_chain_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &program,
                        policy(0),
                        || NOW,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(
                    authorization(&error),
                    Some(Error::ScopeDenied),
                    "{tail}: {error:?}"
                );
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn vertex_and_relationship_actions_share_one_effect_cap_and_rollback_late_errors() {
    lab(0xc4a1_3117, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        let program = chain(
            "UNWIND [1,2] AS x MERGE (a:Visible {p:1}) \
             ON CREATE SET a.q=1 ON MATCH SET a.q=a.q+1 \
             MERGE (b:Visible {p:2}) MERGE (a)-[e:R]->(b) \
             ON CREATE SET e.p=1 ON MATCH SET e.p=e.p+1",
        );
        for limit in [4, 3] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(0).with_mutation_limit(limit),
                    || NOW,
                )
                .await;
            if limit == 4 {
                let (stats, result, _) = result.unwrap();
                assert_eq!((stats.created_vertices, stats.created_edges), (2, 1));
                assert!(result.value.is_empty());
                assert_eq!(result.rows.result_rows, 0);
                assert_eq!(
                    db.edges().unwrap()[0].props,
                    vec![(P, CanonicalScalar::Int(2).into())]
                );
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            } else {
                let error = result.unwrap_err();
                assert!(format!("{error:?}").contains("EffectLimit"), "{error:?}");
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
                assert!(db.edges().unwrap().is_empty());
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let program = chain(
            "UNWIND [1,2] AS x CREATE (marker:Visible {p:x}) \
             MERGE (a:Visible {p:1}) MERGE (b:Visible {p:2}) MERGE (a)-[e:R]->(b) \
             ON CREATE SET e.p=7 ON MATCH SET e.p=7/(2-x)",
        );
        let error = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(0),
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("DivisionByZero"), "{error:?}");
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn relationship_actions_write_carried_endpoints_only_within_scope() {
    // fgdb-at06q: an assignment to a carried endpoint passes the same
    // property scope as a relationship assignment. A denied key refuses the
    // whole statement; an allowed one commits with the creation.
    lab(0xc4a1_3117, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(S);
        for id in 1..=2 {
            seed.create_vertex(
                VId(id as u128),
                vec![L],
                vec![(P, CanonicalScalar::Int(id))],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        for denied in [
            "MATCH (a:Visible {p:1}), (b:Visible {p:2}) MERGE (a)-[:R]->(b) \
             ON CREATE SET b.secret=1",
            "MATCH (a:Visible {p:1}), (b:Visible {p:2}) MERGE (a)-[r:R]->(b) \
             ON CREATE SET r.q=1, a.secret=1",
        ] {
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(denied),
                    policy(10).with_mutation_limit(10),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::ScopeDenied),
                "{denied}: {error:?}"
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
        }
        // Created, then matched: each run takes only its own branch.
        let allowed = chain(
            "MATCH (a:Visible {p:1}), (b:Visible {p:2}) MERGE (a)-[:R]->(b) \
             ON CREATE SET b.q=a.p+10 ON MATCH SET a.q=b.p",
        );
        for created in [true, false] {
            let (stats, _, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &allowed,
                    policy(10).with_mutation_limit(10),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(stats.created_edges, u64::from(created));
        }
        assert_eq!(db.edges().unwrap().len(), 1);
        let props = |id: u128| db.vertex(VId(id)).unwrap().unwrap().props;
        assert_eq!(
            props(1),
            [
                (P, CanonicalScalar::Int(1).into()),
                (Q, CanonicalScalar::Int(2).into())
            ]
        );
        assert_eq!(
            props(2),
            [
                (P, CanonicalScalar::Int(2).into()),
                (Q, CanonicalScalar::Int(11).into())
            ]
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

/// fgdb-r37yy: head/last, list access and UNWIND preserve the vertex identity
/// even though the row's static type is open. Both hosts use the same native
/// action evaluator and keep simultaneous ON-clause before-images.
#[test]
fn relationship_actions_write_computed_endpoints_in_both_hosts() {
    lab(0xc4a1_3711, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        for authorized in [false, true] {
            for endpoints in [
                "WITH ns[0] AS a, ns[1] AS b",
                "WITH head(ns) AS a, last(ns) AS b",
                "UNWIND ns AS a WITH a, a AS b",
            ] {
                let self_loops = endpoints.starts_with("UNWIND");
                let program = chain(&format!(
                    "MATCH (n:Visible) WITH n ORDER BY n.p WITH collect(n) AS ns {endpoints} \
                     MERGE (a)-[r:R]->(b) \
                     ON CREATE SET a.q=b.p+10 ON MATCH SET b.q=a.q+1"
                ));
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(S);
                for id in 1..=2 {
                    seed.create_vertex(
                        VId(id as u128),
                        vec![L],
                        vec![(P, CanonicalScalar::Int(id))],
                    );
                }
                db.write(&commit, seed).await.unwrap();
                for created in [true, false] {
                    let before = db.frontier().unwrap();
                    let stats = if authorized {
                        db.execute_graph_write_chain_authorized(
                            &txn,
                            &query,
                            &commit,
                            &issuer,
                            &token,
                            "main",
                            &program,
                            policy(10).with_mutation_limit(10),
                            || NOW,
                        )
                        .await
                        .unwrap()
                        .0
                    } else {
                        let mut transaction = db.begin(&txn).unwrap();
                        let (stats, _, _) = transaction
                            .execute_graph_write_chain_engine_governed(
                                &mut db,
                                &query,
                                &program,
                                policy(10).with_mutation_limit(10),
                            )
                            .unwrap();
                        transaction.finish(&mut db, &commit).await.unwrap();
                        stats
                    };
                    let expected_edges = if self_loops { 2 } else { 1 };
                    assert_eq!(stats.created_vertices, 0);
                    assert_eq!(
                        stats.created_edges,
                        if created { expected_edges } else { 0 }
                    );
                    assert_eq!(db.frontier().unwrap().0, before.0 + 1);
                    assert_eq!(db.edges().unwrap().len() as u64, expected_edges);
                    let property = |id| {
                        db.vertex(VId(id))
                            .unwrap()
                            .unwrap()
                            .props
                            .into_iter()
                            .find(|(key, _)| *key == Q)
                            .map(|(_, value)| value)
                    };
                    if self_loops {
                        assert_eq!(
                            property(1),
                            Some(CanonicalScalar::Int(if created { 11 } else { 12 }).into())
                        );
                        assert_eq!(
                            property(2),
                            Some(CanonicalScalar::Int(if created { 12 } else { 13 }).into())
                        );
                    } else {
                        assert_eq!(property(1), Some(CanonicalScalar::Int(12).into()));
                        assert_eq!(
                            property(2),
                            (!created).then(|| CanonicalScalar::Int(13).into())
                        );
                    }
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
            }
        }
    });
}

#[test]
fn computed_endpoint_action_fields_keep_one_frozen_input_cut() {
    lab(0xc4a1_3714, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        for authorized in [false, true] {
            let (entry, written) = if authorized { (1, 2) } else { (10, 11) };
            let program = chain(&format!(
                "MATCH (a:Visible {{p:{entry}}}),(b:Visible {{p:2}}) \
                 WITH a,b,[a,a] AS targets UNWIND targets AS target \
                 MERGE (a)-[r:R]->(b) \
                 ON CREATE SET target.p=target.p+1,target.q=target.p \
                 ON MATCH SET target.p=target.p+1,target.q=target.p"
            ));
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(S);
            for id in 1..=2 {
                seed.create_vertex(
                    VId(id as u128),
                    vec![L],
                    vec![(P, CanonicalScalar::Int(id))],
                );
            }
            db.write(&commit, seed).await.unwrap();
            let stats = if authorized {
                db.execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(10).with_mutation_limit(10),
                    || NOW,
                )
                .await
                .unwrap()
                .0
            } else {
                let mut transaction = db.begin(&txn).unwrap();
                // The input cut includes the caller's uncommitted prefix,
                // not just the database basis or this statement's effects.
                let mut prior = WriteBatch::new(S);
                prior.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(entry)));
                transaction.write(&mut db, prior).unwrap();
                let (stats, _, _) = transaction
                    .execute_graph_write_chain_engine_governed(
                        &mut db,
                        &query,
                        &program,
                        policy(10).with_mutation_limit(10),
                    )
                    .unwrap();
                transaction.finish(&mut db, &commit).await.unwrap();
                stats
            };
            assert_eq!(stats.created_edges, 1);
            assert_eq!(db.edges().unwrap().len(), 1);
            // Both input occurrences read the entry value. Reading the growing
            // overlay would add twice; reading within an assignment clause
            // would incorrectly make q follow that clause's new p.
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props,
                [
                    (P, CanonicalScalar::Int(written).into()),
                    (Q, CanonicalScalar::Int(entry).into()),
                ]
            );
            assert_eq!(
                db.vertex(VId(2)).unwrap().unwrap().props,
                [(P, CanonicalScalar::Int(2).into())]
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn open_relationship_action_targets_keep_null_noops_and_late_type_rollback() {
    lab(0xc4a1_3712, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        for authorized in [false, true] {
            for last in ["null", "1", "{p:1}"] {
                let program = chain(&format!(
                    "MATCH (a:Visible {{p:1}}),(b:Visible {{p:2}}) \
                     WITH a,b,[a,{last}] AS targets UNWIND targets AS target \
                     MERGE (a)-[r:R]->(b) ON CREATE SET target.q=7 ON MATCH SET target.q=8"
                ));
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(S);
                for id in 1..=2 {
                    seed.create_vertex(
                        VId(id as u128),
                        vec![L],
                        vec![(P, CanonicalScalar::Int(id))],
                    );
                }
                db.write(&commit, seed).await.unwrap();
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let result = if authorized {
                    db.execute_graph_write_chain_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &program,
                        policy(10).with_mutation_limit(10),
                        || NOW,
                    )
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{error:?}"))
                } else {
                    let mut transaction = db.begin(&txn).unwrap();
                    let result = transaction
                        .execute_graph_write_chain_engine_governed(
                            &mut db,
                            &query,
                            &program,
                            policy(10).with_mutation_limit(10),
                        )
                        .map(|_| ())
                        .map_err(|error| format!("{error:?}"));
                    // Even a refused statement leaves a usable transaction.
                    // Finishing it proves the statement guard, rather than an
                    // outer transaction abort, removed the earlier row's effects.
                    transaction.finish(&mut db, &commit).await.unwrap();
                    result
                };
                if last == "null" {
                    result.unwrap();
                    assert_eq!(db.edges().unwrap().len(), 1);
                    assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                    assert!(
                        db.vertex(VId(1))
                            .unwrap()
                            .unwrap()
                            .props
                            .contains(&(Q, CanonicalScalar::Int(7).into()))
                    );
                    assert!(
                        db.vertex(VId(2))
                            .unwrap()
                            .unwrap()
                            .props
                            .iter()
                            .all(|(key, _)| *key != Q)
                    );
                } else {
                    let error = result.unwrap_err();
                    assert!(error.contains("InputSchema"), "{error}");
                    assert_eq!(
                        (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap()
                        ),
                        before
                    );
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn computed_endpoint_actions_keep_selected_branch_authority_and_rollback() {
    lab(0xc4a1_3713, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(S);
        for id in 1..=2 {
            seed.create_vertex(
                VId(id as u128),
                vec![L],
                vec![(P, CanonicalScalar::Int(id))],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        for actions in [
            "ON CREATE SET a.secret=9",
            "ON CREATE SET a.q=1 ON MATCH SET b.secret=9",
        ] {
            let program = chain(&format!(
                "MATCH (n:Visible) WITH n ORDER BY n.p WITH collect(n) AS ns \
                 WITH head(ns) AS a,last(ns) AS b \
                 WITH a,b,[1,2] AS occurrences UNWIND occurrences AS occurrence \
                 MERGE (a)-[r:R]->(b) {actions}"
            ));
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(10).with_mutation_limit(10),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(Error::ScopeDenied), "{error:?}");
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn computed_endpoint_fields_share_native_and_signed_budgets_without_hidden_work() {
    async fn attempt(
        contexts: &PurposeContexts,
        issuer: &Authority,
        hidden: bool,
        payload: usize,
        budget: GraphInsertPolicy,
        signed_work: u64,
    ) -> Option<GraphInsertStats> {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(S);
        for id in 1..=2 {
            let mut properties = vec![
                (P, CanonicalScalar::Int(id)),
                (Q, CanonicalScalar::bytes(vec![0x37; payload]).unwrap()),
            ];
            if hidden {
                properties.push((SECRET, CanonicalScalar::bytes(vec![0x99; 16_384]).unwrap()));
            }
            seed.create_vertex(VId(id as u128), vec![L], properties);
        }
        if hidden {
            for id in 3..=8 {
                seed.create_vertex(
                    VId(id as u128),
                    vec![HIDDEN],
                    vec![(P, CanonicalScalar::Int(id))],
                );
                seed.add_edge(EId(id as u128), VId(1), VId(id as u128), vec![]);
            }
        }
        db.write(&commit, seed).await.unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let token = issuer
            .issue_at(&grant(Rights::ReadWrite, 10), NOW)
            .unwrap()
            .attenuate(Restriction::MaxWork(signed_work))
            .unwrap();
        let program = chain(
            "MATCH (a:Visible {p:1}),(b:Visible {p:2}) \
             WITH a,b,[a,a] AS targets UNWIND targets AS target \
             MERGE (a)-[r:R]->(b) \
             ON CREATE SET target.q=coalesce(target.secret,target.q) \
             ON MATCH SET target.q=coalesce(target.secret,target.q)",
        );
        let result = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                issuer,
                &token,
                "main",
                &program,
                budget,
                || NOW,
            )
            .await;
        assert_eq!(txn.outstanding_obligations(), 0);
        match result {
            Ok((stats, rows, _)) => {
                assert!(rows.value.is_empty());
                assert_eq!((stats.created_vertices, stats.created_edges), (0, 1));
                assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                assert_eq!(db.vertices().unwrap(), before.1);
                assert_eq!(db.edges().unwrap().len(), before.2.len() + 1);
                Some(stats)
            }
            Err(error) => {
                eprintln!(
                    "computed endpoint refusal (hidden={hidden}, payload={payload}): {error:?}"
                );
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before,
                    "a later field-read refusal must discard the first occurrence's relationship",
                );
                None
            }
        }
    }

    lab(0xc4a1_3715, |contexts| async move {
        let issuer = authority();
        let budget = policy(10).with_mutation_limit(10);
        let small = attempt(&contexts, &issuer, false, 8, budget, 1_000_000)
            .await
            .unwrap();
        let measured = attempt(&contexts, &issuer, false, 8_192, budget, 1_000_000)
            .await
            .unwrap();
        assert!(measured.evaluator.work_units > small.evaluator.work_units);
        assert!(measured.evaluator.scratch_entries > small.evaluator.scratch_entries);
        assert_eq!(
            attempt(&contexts, &issuer, true, 8_192, budget, 1_000_000)
                .await
                .unwrap(),
            measured,
            "hidden payload and physical probes cannot change native logical usage",
        );
        let mut exact = budget;
        exact.query.evaluator.max_work_units = measured.evaluator.work_units;
        exact.query.evaluator.max_scratch_entries = measured.evaluator.scratch_entries;
        for hidden in [false, true] {
            assert_eq!(
                attempt(&contexts, &issuer, hidden, 8_192, exact, 1_000_000).await,
                Some(measured)
            );
            for scratch in [false, true] {
                let mut short = exact;
                if scratch {
                    short.query.evaluator.max_scratch_entries -= 1;
                } else {
                    short.query.evaluator.max_work_units -= 1;
                }
                assert!(
                    attempt(&contexts, &issuer, hidden, 8_192, short, 1_000_000)
                        .await
                        .is_none()
                );
            }
        }
        // The scoped point reader and effects share the original signed work
        // permit. Find its exact boundary without fixing an internal event count.
        let (mut low, mut high) = (0, 1_000_000);
        while low < high {
            let middle = low + (high - low) / 2;
            if attempt(&contexts, &issuer, false, 8_192, exact, middle)
                .await
                .is_some()
            {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        assert!(low > 0);
        for hidden in [false, true] {
            assert_eq!(
                attempt(&contexts, &issuer, hidden, 8_192, exact, low).await,
                Some(measured)
            );
            assert!(
                attempt(&contexts, &issuer, hidden, 8_192, exact, low - 1)
                    .await
                    .is_none()
            );
        }
    });
}

#[test]
fn whole_pair_reduction_preserves_parallel_orientation_and_private_bag_rows() {
    lab(0xc4a1_3101, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 5), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=4 {
            seed.create_vertex(
                VId(id as u128),
                vec![L],
                vec![(P, CanonicalScalar::Int(id))],
            );
        }
        // Pair order differs from edge order, and one input pair occurs twice.
        for (edge, destination) in [(11, 4), (12, 2), (13, 3), (14, 2)] {
            seed.add_edge(EId(edge), VId(1), VId(destination), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let forward = EId((1_u128 << 100) + 21);
        let reverse = EId((1_u128 << 100) + 22);
        let mut existing = WriteBatch::new(S);
        existing.add_edge(forward, VId(1), VId(3), vec![(P, CanonicalScalar::Int(7))]);
        existing.add_edge(reverse, VId(3), VId(1), vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, existing).await.unwrap();
        let before = db.frontier().unwrap();
        let program = chain(
            "MATCH (a:Visible)-[:R]->(b:Visible) CREATE (marker:Visible {p:b.p}) \
             MERGE (a)-[s:S {p:7}]-(b) \
             RETURN b.p AS destination, s AS edge, startNode(s) AS source, endNode(s) AS target \
             ORDER BY destination, edge",
        );
        let mut bounded = policy(5);
        bounded.max_edges = 2;
        let (stats, result, completion) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                bounded,
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (4, 2));
        assert_eq!(result.rows.result_rows, 5);
        let rows: Vec<_> = result.value.iter().map(GraphValueRow::values).collect();
        assert_eq!(
            rows.iter().map(|row| row[0].clone()).collect::<Vec<_>>(),
            vec![int(2), int(2), int(3), int(3), int(4)]
        );
        assert_eq!(rows[0][1], rows[1][1], "duplicate pairs share one creation");
        assert_eq!(
            rows[2],
            &[
                int(3),
                GraphValue::Edge(forward),
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(3))
            ]
        );
        assert_eq!(
            rows[3],
            &[
                int(3),
                GraphValue::Edge(reverse),
                GraphValue::Vertex(VId(3)),
                GraphValue::Vertex(VId(1))
            ]
        );
        assert_eq!(
            db.edges()
                .unwrap()
                .iter()
                .filter(|edge| edge.entry.relation == S)
                .count(),
            4
        );
        let frontier = db.frontier().unwrap();
        assert_eq!(frontier.0, before.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: frontier
            }
        );

        // Now MERGE is a private intermediate owner: each parallel match and
        // repeated source occurrence must reach the following CREATE.
        let program = chain(
            "MATCH (a:Visible)-[:R]->(b:Visible) MERGE (a)-[s:S {p:7}]-(b) \
             CREATE (copy:Visible {p:b.p}) RETURN copy.p AS destination ORDER BY destination",
        );
        bounded.max_edges = 0;
        let (stats, result, completion) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                bounded,
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (5, 0));
        assert_eq!(
            result
                .value
                .iter()
                .map(|row| row.values()[0].clone())
                .collect::<Vec<_>>(),
            vec![int(2), int(2), int(3), int(3), int(4)]
        );
        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: db.frontier().unwrap(),
            }
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[allow(clippy::too_many_arguments)]
async fn hidden_attempt(
    commit: &CommitCx,
    txn: &TxnCx,
    query: &QueryCx,
    issuer: &Authority,
    hidden: bool,
    returned: bool,
    native_work: u64,
    signed_work: u64,
) -> Option<(GraphInsertStats, Vec<GraphValueRow>)> {
    let mut db = Database::open_memory(commit, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    for id in 1..=2 {
        let mut properties = vec![(P, CanonicalScalar::Int(id))];
        if hidden {
            properties.push((SECRET, CanonicalScalar::Int(99)));
        }
        seed.create_vertex(VId(id as u128), vec![L], properties);
    }
    seed.add_edge(EId(11), VId(1), VId(2), vec![]);
    if hidden {
        seed.create_vertex(VId(3), vec![HIDDEN], vec![(P, CanonicalScalar::Int(3))]);
        seed.create_vertex(VId(4), vec![HIDDEN], vec![(P, CanonicalScalar::Int(4))]);
        seed.add_edge(EId(12), VId(1), VId(3), vec![]);
        seed.add_edge(EId(13), VId(4), VId(2), vec![]);
    }
    db.write(commit, seed).await.unwrap();
    let mut existing = WriteBatch::new(S);
    let mut properties = vec![(P, CanonicalScalar::Int(7))];
    if hidden {
        properties.push((SECRET, CanonicalScalar::Int(88)));
        existing.add_edge(EId(22), VId(3), VId(2), vec![(P, CanonicalScalar::Int(7))]);
    }
    existing.add_edge(EId(21), VId(1), VId(2), properties);
    db.write(commit, existing).await.unwrap();
    let before = (
        db.frontier().unwrap(),
        db.vertices().unwrap(),
        db.edges().unwrap(),
    );
    let rows = u64::from(returned);
    let token = issuer
        .issue_at(&grant(Rights::ReadWrite, rows), NOW)
        .unwrap()
        .attenuate(Restriction::MaxWork(signed_work))
        .unwrap();
    let program = chain(if returned {
        "MATCH (a:Visible)-[:R]->(b:Visible) CREATE (marker:Visible {p:a.p}) \
         MERGE (a)-[s:S {p:7}]->(b) RETURN s.p AS relationship, a.p AS source, b.p AS target"
    } else {
        "MATCH (a:Visible)-[:R]->(b:Visible) CREATE (marker:Visible {p:a.p}) \
         MERGE (a)-[s:S {p:7}]->(b)"
    });
    let mut bounded = policy(rows);
    bounded.query.evaluator.max_work_units = native_work;
    bounded.max_edges = 0;
    let result = db
        .execute_graph_write_chain_authorized(
            txn,
            query,
            commit,
            issuer,
            &token,
            "main",
            &program,
            bounded,
            || NOW,
        )
        .await;
    assert_eq!(txn.outstanding_obligations(), 0);
    match result {
        Ok((stats, result, completion)) => {
            assert_eq!((stats.created_vertices, stats.created_edges), (1, 0));
            assert_eq!(result.rows.result_rows, rows);
            assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: db.frontier().unwrap(),
                }
            );
            if returned {
                assert_eq!(result.value.len(), 1);
                assert_eq!(result.value[0].values(), &[int(7), int(1), int(2)]);
            } else {
                assert!(result.value.is_empty());
            }
            Some((stats, result.value))
        }
        Err(_) => {
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            None
        }
    }
}

#[test]
fn hidden_records_and_fields_do_not_change_merge_chain_work_or_admission() {
    lab(0xc4a1_3102, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        for returned in [false, true] {
            let visible = hidden_attempt(
                &commit, &txn, &query, &issuer, false, returned, 1_000_000, 1_000_000,
            )
            .await
            .unwrap();
            let masked = hidden_attempt(
                &commit, &txn, &query, &issuer, true, returned, 1_000_000, 1_000_000,
            )
            .await
            .unwrap();
            assert_eq!(
                visible, masked,
                "hidden physical data changed logical execution"
            );
            let native = visible.0.evaluator.work_units;
            assert!(native > 0);
            for hidden in [false, true] {
                assert!(
                    hidden_attempt(
                        &commit, &txn, &query, &issuer, hidden, returned, native, 1_000_000
                    )
                    .await
                    .is_some()
                );
                assert!(
                    hidden_attempt(
                        &commit,
                        &txn,
                        &query,
                        &issuer,
                        hidden,
                        returned,
                        native - 1,
                        1_000_000
                    )
                    .await
                    .is_none()
                );
            }
            // Find the real signed-work boundary rather than prescribing an
            // implementation-specific number. All failures also prove rollback.
            let (mut low, mut high) = (0, 1_000_000);
            while low < high {
                let middle = low + (high - low) / 2;
                if hidden_attempt(
                    &commit, &txn, &query, &issuer, false, returned, 1_000_000, middle,
                )
                .await
                .is_some()
                {
                    high = middle;
                } else {
                    low = middle + 1;
                }
            }
            assert!(low > 0);
            for hidden in [false, true] {
                assert!(
                    hidden_attempt(
                        &commit, &txn, &query, &issuer, hidden, returned, 1_000_000, low
                    )
                    .await
                    .is_some()
                );
                assert!(
                    hidden_attempt(
                        &commit,
                        &txn,
                        &query,
                        &issuer,
                        hidden,
                        returned,
                        1_000_000,
                        low - 1
                    )
                    .await
                    .is_none()
                );
            }
        }
    });
}

#[test]
fn merge_tail_limits_keep_effects_and_late_failures_restore_the_creation_prefix() {
    lab(0xc4a1_3103, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        for rows in [0, 1] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let token = issuer
                .issue_at(&grant(Rights::ReadWrite, rows), NOW)
                .unwrap();
            let program = chain(&format!(
                "UNWIND [1,2] AS x CREATE (a:Visible {{p:x}}), (b:Visible {{p:x+10}}) \
                 MERGE (a)-[e:R {{p:7}}]->(b) RETURN count(e) AS occurrences LIMIT {rows}"
            ));
            let (stats, result, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(rows),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (4, 2));
            assert_eq!(result.value.len() as u64, rows);
            assert_eq!(result.rows.result_rows, rows);
            if rows == 1 {
                assert_eq!(result.value[0].values(), &[int(2)]);
            }
            assert_eq!(
                (db.vertices().unwrap().len(), db.edges().unwrap().len()),
                (4, 2)
            );
        }
        for (tail, edges) in [
            ("RETURN count(e) AS occurrences", 0),
            ("RETURN 1 / (e.p - 7) AS invalid LIMIT 0", 100),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let rows = u64::from(!tail.ends_with("LIMIT 0"));
            let token = issuer
                .issue_at(&grant(Rights::ReadWrite, rows), NOW)
                .unwrap();
            let program = chain(&format!(
                "CREATE (a:Visible {{p:1}}), (b:Visible {{p:2}}) \
                 MERGE (a)-[e:R {{p:7}}]->(b) {tail}"
            ));
            let mut bounded = policy(rows);
            bounded.max_edges = edges;
            assert!(
                db.execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await
                .is_err()
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn explicit_hidden_bindings_and_write_only_merge_refuse_the_entire_chain() {
    lab(0xc4a1_3104, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        for (tail, rights, expected) in [
            (
                "MERGE (a)-[e:R]->(b) RETURN e.secret",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "MERGE (a)-[e:R]->(b) RETURN a.secret",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "MERGE (a)-[:R {secret:7}]->(b)",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            // A hidden key whose value the row computes (fgdb-at06q).
            (
                "WITH a, b, 7 AS s MERGE (a)-[:R {secret: s}]->(b)",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "WITH a, b, 7 AS s MERGE (a)-[:R {p: s, secret: s + 1}]->(b)",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "MERGE (a)-[e:R]->(b) CREATE (c:Visible {p:e.secret})",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "MERGE (a)-[:R]->(b)",
                Rights::Write,
                Error::PermissionDenied,
            ),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let token = issuer.issue_at(&grant(rights, 10), NOW).unwrap();
            let program = chain(&format!("CREATE (a:Visible), (b:Visible) {tail}"));
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(10),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(expected), "{tail}: {error:?}");
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn frozen_parameter_merges_preserve_exact_keys_and_reopen_one_idempotent_import() {
    lab(0xc4a1_3105, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let large = 1_i64 << 53;
        let float = |value| GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)));
        let pairs = vec![
            (int(1), int(2)),
            (int(2), int(3)),
            (int(1), int(2)),
            (int(3), int(3)),
            (float(1.0), int(2)),
            (int(large + 1), int(0)),
            (float(large as f64), int(0)),
        ];
        let parameters = GqlParameters::new()
            .with_list(
                "rows",
                pairs
                    .into_iter()
                    .map(|(source, target)| {
                        GraphValue::map(vec![("s".into(), source), ("d".into(), target)]).unwrap()
                    })
                    .collect(),
            )
            .unwrap();
        let original = parameters.canonical_bytes();
        let statement = "UNWIND $rows AS row MERGE (a:Visible {p:row.s}) \
             MERGE (b:Visible {p:row.d}) MERGE (a)-[:R]->(b)";
        let program = chain_with(statement, &parameters);
        let (stats, result, completion) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (6, 5));
        assert!(result.value.is_empty());
        assert_eq!(result.rows.result_rows, 0);
        let frontier = db.frontier().unwrap();
        assert_eq!(frontier.0, before.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: frontier
            }
        );
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        let property = |vertex| {
            vertices.iter().find(|row| row.vid == vertex).unwrap().props[0]
                .1
                .clone()
        };
        let integer = |value| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value));
        let mut actual: Vec<_> = edges
            .iter()
            .map(|edge| (property(edge.entry.src), property(edge.entry.dst)))
            .collect();
        let mut expected = vec![
            (integer(1), integer(2)),
            (integer(2), integer(3)),
            (integer(3), integer(3)),
            (integer(large + 1), integer(0)),
            (
                CanonicalScalar::Float(CanonicalF64::new(large as f64)).into(),
                integer(0),
            ),
        ];
        actual.sort();
        expected.sort();
        assert_eq!(
            actual, expected,
            "rounded numeric buckets must not become equality"
        );
        let mut no_creation = policy(0);
        no_creation.max_vertices = 0;
        no_creation.max_edges = 0;
        let (stats, result, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                no_creation,
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
        assert!(result.value.is_empty());
        let empty = chain_with(
            statement,
            &GqlParameters::new().with_list("rows", Vec::new()).unwrap(),
        );
        let (stats, result, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &empty,
                no_creation,
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
        assert!(result.value.is_empty());
        assert_eq!(parameters.canonical_bytes(), original);
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(db.edges().unwrap(), edges);
        assert_eq!(txn.outstanding_obligations(), 0);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(db.edges().unwrap(), edges);
    });
}

#[test]
fn vertex_merge_bags_keep_all_visible_matches_before_final_paging() {
    lab(0xc4a1_3106, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        for (tail, public_rows) in [
            ("RETURN a ORDER BY a", 4),
            ("RETURN count(*) AS copies", 1),
            ("RETURN count(*) AS copies LIMIT 0", 0),
        ] {
            let token = issuer
                .issue_at(&grant(Rights::ReadWrite, public_rows), NOW)
                .unwrap();
            let program = chain(&format!(
                "UNWIND [7,7] AS x MERGE (a:Visible {{p:x}}) \
                 MERGE (b:Visible {{p:9}}) {tail}"
            ));
            let mut visible = None;
            for hidden in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(R);
                for id in [1, 3] {
                    let mut properties = vec![(P, CanonicalScalar::Int(7))];
                    if hidden {
                        properties.push((SECRET, CanonicalScalar::Int(99)));
                    }
                    seed.create_vertex(VId(id), vec![L], properties);
                }
                if hidden {
                    seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(7))]);
                }
                db.write(&commit, seed).await.unwrap();
                let before = db.frontier().unwrap();
                let (stats, result, _) = db
                    .execute_graph_write_chain_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &program,
                        policy(public_rows),
                        || NOW,
                    )
                    .await
                    .unwrap();
                assert_eq!((stats.created_vertices, stats.created_edges), (1, 0));
                assert_eq!(result.rows.result_rows, public_rows);
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
                if public_rows == 4 {
                    assert_eq!(
                        result
                            .value
                            .iter()
                            .map(GraphValueRow::values)
                            .collect::<Vec<_>>(),
                        vec![
                            &[GraphValue::Vertex(VId(1))][..],
                            &[GraphValue::Vertex(VId(1))][..],
                            &[GraphValue::Vertex(VId(3))][..],
                            &[GraphValue::Vertex(VId(3))][..],
                        ],
                    );
                } else if public_rows == 1 {
                    assert_eq!(result.value[0].values(), &[int(4)]);
                } else {
                    assert!(result.value.is_empty());
                }
                let observed = (stats, result.value);
                if let Some(expected) = &visible {
                    assert_eq!(
                        &observed, expected,
                        "hidden matches and fields cannot affect work"
                    );
                } else {
                    visible = Some(observed);
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn frozen_import_work_does_not_rescan_the_growing_creation_prefix() {
    lab(0xc4a1_3107, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        for count in [64_i64, 128] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let input = GqlParameters::new()
                .with_list(
                    "rows",
                    (0..count * 2)
                        .map(|at| {
                            GraphValue::map(vec![
                                ("s".into(), int(at % count)),
                                ("d".into(), int((at + 1) % count)),
                            ])
                            .unwrap()
                        })
                        .collect(),
                )
                .unwrap();
            let program = chain_with(
                "UNWIND $rows AS row MERGE (a:Visible {p:row.s}) \
                 MERGE (b:Visible {p:row.d}) MERGE (a)-[:R]->(b)",
                &input,
            );
            // A sequential growing-overlay scan exceeds this linear record
            // allowance. The entry snapshot plus held creations stays within it.
            let bounded = GraphInsertPolicy::new(
                GqlQueryPolicy::new(16 * count as u64, 0, 1_000_000, 100_000),
                count as u64,
                count as u64,
            );
            let (stats, result, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(
                (stats.created_vertices, stats.created_edges),
                (count as u64, count as u64)
            );
            assert!(stats.selection.snapshot_records <= 16 * count as u64);
            assert!(result.value.is_empty());
            assert_eq!(db.vertices().unwrap().len(), count as usize);
            assert_eq!(db.edges().unwrap().len(), count as usize);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn frozen_on_create_changes_are_seen_by_later_merges_and_final_creation() {
    lab(0xc4a1_3108, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let program = chain(
            "UNWIND [1,2] AS x MERGE (a:Visible {p:x}) ON CREATE SET a.q=7,a.p=x+1 \
             MERGE (b:Visible {p:x}) CREATE (a)-[:R]->(b)",
        );
        let (stats, result, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(0).with_mutation_limit(2),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (2, 2));
        assert!(result.value.is_empty());
        let vertices = db.vertices().unwrap();
        let first = vertices
            .iter()
            .find(|vertex| {
                vertex.props
                    == vec![
                        (P, CanonicalScalar::Int(2).into()),
                        (Q, CanonicalScalar::Int(7).into()),
                    ]
            })
            .unwrap();
        let second = vertices
            .iter()
            .find(|vertex| vertex.props == vec![(P, CanonicalScalar::Int(1).into())])
            .unwrap();
        let mut pairs: Vec<_> = db
            .edges()
            .unwrap()
            .iter()
            .map(|edge| (edge.entry.src, edge.entry.dst))
            .collect();
        pairs.sort();
        let mut expected = vec![(first.vid, first.vid), (first.vid, second.vid)];
        expected.sort();
        assert_eq!(pairs, expected);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn duplicate_merge_actions_run_per_occurrence_with_clause_entry_rhs_values() {
    lab(0xc4a1_3109, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 3), NOW).unwrap();
        let program = chain(
            "UNWIND [1,1,1] AS x MERGE (a:Visible {p:x}) \
             ON CREATE SET a.q=0 ON MATCH SET a.q=a.q+1 SET a.q=a.q+10 \
             MERGE (b:Visible {p:2}) RETURN a.q AS q",
        );
        for limit in [6, 5] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(3).with_mutation_limit(limit),
                    || NOW,
                )
                .await;
            if limit == 6 {
                let (stats, result, _) = result.unwrap();
                assert_eq!(stats.created_vertices, 2);
                assert_eq!(result.value.len(), 3);
                for row in result.value {
                    assert_eq!(row.values(), &[int(32)]);
                }
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            } else {
                assert!(
                    result.is_err(),
                    "all six original actions share the statement cap"
                );
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L],
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(7))],
        );
        db.write(&commit, seed).await.unwrap();
        let program = chain(
            "CREATE (marker:Visible {p:99}) MERGE (a:Visible {p:1}) \
             ON MATCH SET a.p=a.q,a.q=a.p RETURN a.p AS p,a.q AS q",
        );
        let (_, result, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1).with_mutation_limit(2),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            result.value[0].values(),
            &[int(7), int(1)],
            "SET RHS reads precede the clause's writes"
        );
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(7).into()),
                (Q, CanonicalScalar::Int(1).into())
            ]
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn vertex_merge_scope_checks_cover_noop_actions_keys_and_carried_fields() {
    lab(0xc4a1_3110, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(1)),
                (SECRET, CanonicalScalar::Int(99)),
            ],
        );
        db.write(&commit, seed).await.unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        for text in [
            "CREATE (marker:Visible) MERGE (a:Visible {p:1}) ON MATCH SET a.secret=99",
            "CREATE (marker:Visible) MERGE (a:Visible {p:1}) ON MATCH SET a:Hidden",
            "CREATE (marker:Visible) MERGE (a:Visible {secret:99})",
            "CREATE (marker:Visible) MERGE (a:Hidden {p:1})",
            "CREATE (marker:Visible) MERGE (a:Visible {p:1}) RETURN a.secret",
            "MERGE (a:Visible {p:4}) ON CREATE SET a.secret=99 MERGE (b:Visible {p:5})",
            "MERGE (a:Visible {p:4}) ON CREATE SET a:Hidden MERGE (b:Visible {p:5})",
        ] {
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    policy(10),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::ScopeDenied),
                "{text}: {error:?}"
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        // An unselected ON branch is not an intent. The same hidden field
        // that refuses above must not deny a branch that never executes.
        for (text, created) in [
            (
                "MERGE (a:Visible {p:1}) ON CREATE SET a.secret=0 \
                 MERGE (b:Visible {p:1}) RETURN count(*) AS copies",
                0,
            ),
            (
                "MERGE (a:Visible {p:4}) ON MATCH SET a.secret=0 \
                 MERGE (b:Visible {p:4}) RETURN count(*) AS copies",
                1,
            ),
        ] {
            let (stats, result, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    policy(1).with_mutation_limit(0),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(stats.created_vertices, created);
            assert_eq!(result.value[0].values(), &[int(1)]);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap(), before.1[0]);
    });
}

#[test]
fn vertex_merge_native_limits_and_late_expiry_or_output_refusal_abort_every_step() {
    lab(0xc4a1_3111, |contexts| async move {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain(
            "UNWIND [1,2] AS x MERGE (a:Visible {p:x}) \
             MERGE (b:Visible {p:x+10}) MERGE (a)-[e:R]->(b) RETURN count(e) AS copies",
        );
        let mut measured = None;
        for attempt in 0..6 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut bounded = policy(1);
            if let Some((work, scratch)) = measured {
                bounded.query = GqlQueryPolicy::new(
                    10_000,
                    1,
                    work - u64::from(attempt == 2),
                    scratch - u64::from(attempt == 3),
                );
            }
            if attempt == 4 {
                bounded.max_vertices = 3;
            } else if attempt == 5 {
                bounded.max_edges = 1;
            }
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await;
            if attempt < 2 {
                let (stats, result, _) = result.unwrap();
                assert_eq!((stats.created_vertices, stats.created_edges), (4, 2));
                assert_eq!(result.value[0].values(), &[int(2)]);
                let observed = (
                    result.evaluator.work_units,
                    result.evaluator.scratch_entries,
                );
                assert!(observed.0 > 0 && observed.1 > 0);
                if let Some(expected) = measured {
                    assert_eq!(observed, expected, "exact cumulative limits still succeed");
                }
                measured = Some(observed);
            } else {
                assert!(result.is_err(), "attempt {attempt} must refuse");
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
                assert!(db.edges().unwrap().is_empty());
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        for expire in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let now = Arc::new(AtomicU64::new(NOW));
            let admission_clock = Arc::clone(&now);
            let mut called = 0;
            let error = db
                .execute_graph_write_chain_authorized_checked(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1),
                    || now.load(Ordering::Relaxed),
                    |rows, meter| {
                        called += 1;
                        assert_eq!(rows.len(), 1);
                        assert_eq!(rows[0].values(), &[int(2)]);
                        meter(1)?;
                        if expire {
                            admission_clock.store(10_000, Ordering::Relaxed);
                            Ok(())
                        } else {
                            Err(WriteTxnError::Authorization(Error::ScopeDenied))
                        }
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(called, 1, "the complete tentative result was reached");
            assert_eq!(
                authorization(&error),
                Some(if expire {
                    Error::Expired
                } else {
                    Error::ScopeDenied
                })
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn hidden_merge_action_inputs_are_null_before_reading_and_do_not_change_work() {
    lab(0xc4a1_3112, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain(
            "CREATE (marker:Visible {p:2}) MERGE (a:Visible {p:1}) \
             ON MATCH SET a.q=a.secret MERGE (b:Visible {p:2}) RETURN a.q AS q",
        );
        let mut absent = None;
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            let mut properties = vec![(P, CanonicalScalar::Int(1))];
            if hidden {
                properties.push((SECRET, CanonicalScalar::Int(99)));
            }
            properties.push((Q, CanonicalScalar::Int(7)));
            seed.create_vertex(VId(1), vec![L], properties);
            db.write(&commit, seed).await.unwrap();
            let before = db.frontier().unwrap();
            let (stats, result, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1).with_mutation_limit(1),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (1, 0));
            assert_eq!(
                result.value[0].values(),
                &[GraphValue::Scalar(CanonicalScalar::Null)]
            );
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            let stored = db.vertex(VId(1)).unwrap().unwrap();
            assert_eq!(
                stored
                    .props
                    .iter()
                    .find(|(key, _)| *key == Q)
                    .map(|(_, value)| value),
                None,
                "masked NULL removes the old allowed property"
            );
            assert_eq!(
                stored
                    .props
                    .iter()
                    .find(|(key, _)| *key == SECRET)
                    .map(|(_, value)| value),
                hidden.then_some(&CanonicalPropertyValue::Scalar(CanonicalScalar::Int(99))),
                "the hidden source field must remain unchanged",
            );
            let mut visible_graph: Vec<_> = db
                .vertices()
                .unwrap()
                .into_iter()
                .map(|vertex| {
                    let properties: Vec<_> = vertex
                        .props
                        .into_iter()
                        .filter(|(key, _)| *key != SECRET)
                        .collect();
                    (vertex.labels, properties)
                })
                .collect();
            visible_graph.sort();
            let observed = (stats, result.value, visible_graph);
            if let Some(expected) = &absent {
                assert_eq!(
                    &observed, expected,
                    "hidden and absent inputs have identical observable work and effects"
                );
            } else {
                absent = Some(observed);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn terminal_vertex_merge_rows_stay_private_with_and_without_frozen_execution() {
    lab(0xc4a1_3113, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        for actions in ["", " ON MATCH SET a.secret=99"] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let program = chain(&format!(
                "UNWIND [1,2] AS x MERGE (a:Visible {{p:x}}){actions} \
                 MERGE (b:Visible {{p:x+10}})"
            ));
            assert!(!program.returns_rows());
            let mut admitted = 0;
            let (stats, result, completion) = db
                .execute_graph_write_chain_authorized_checked(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(0).with_mutation_limit(0),
                    || NOW,
                    |rows, meter| {
                        admitted += 1;
                        assert!(
                            rows.is_empty(),
                            "private continuation identities must not reach the host"
                        );
                        meter(1)
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                admitted, 1,
                "no-output statements still admit their empty representation"
            );
            assert_eq!((stats.created_vertices, stats.created_edges), (4, 0));
            assert_eq!(stats.selection.result_rows, 0);
            assert_eq!(result.rows.result_rows, 0);
            assert!(result.value.is_empty());
            assert_eq!(db.vertices().unwrap().len(), 4);
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: db.frontier().unwrap()
                }
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn matched_relationship_merge_masks_hidden_inputs_and_refuses_denied_actions_atomically() {
    lab(0xc4a1_3119, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 2), NOW).unwrap();
        let program = chain(
            "CREATE (a:Visible {p:1}) WITH a UNWIND [10,20,30] AS k \
             MATCH (b {p:k}) MERGE (a)-[r:R]->(b) ON CREATE SET r.p=b.p \
             RETURN b.p AS target,r.p AS weight ORDER BY target",
        );
        let mut absent = None;
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(S);
            for id in [10_u128, 20] {
                seed.create_vertex(VId(id), vec![L], vec![(P, CanonicalScalar::Int(id as i64))]);
            }
            if hidden {
                seed.create_vertex(VId(30), vec![HIDDEN], vec![(P, CanonicalScalar::Int(30))]);
            }
            db.write(&commit, seed).await.unwrap();
            let before = db.frontier().unwrap();
            let (stats, result, completion) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(2).with_mutation_limit(10),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (1, 2));
            assert_eq!(result.rows.result_rows, 2);
            assert_eq!(
                result
                    .value
                    .iter()
                    .map(GraphValueRow::values)
                    .collect::<Vec<_>>(),
                vec![&[int(10), int(10)][..], &[int(20), int(20)][..]]
            );
            assert_eq!(db.edges().unwrap().len(), 2);
            assert!(
                db.edges()
                    .unwrap()
                    .iter()
                    .all(|edge| edge.entry.dst != VId(30))
            );
            let frontier = db.frontier().unwrap();
            assert_eq!(frontier.0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: frontier
                }
            );
            let observed = (stats, result.rows, result.evaluator, result.value);
            if let Some(expected) = &absent {
                assert_eq!(
                    &observed, expected,
                    "hidden and absent MATCH inputs have identical visible answers and charged work"
                );
            } else {
                absent = Some(observed);
            }

            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            for action in ["r.secret=9", "b.secret=9"] {
                let denied = chain(&format!(
                    "CREATE (a:Visible {{p:100}}) WITH a MATCH (b:Visible {{p:10}}) \
                     MERGE (a)-[r:R]->(b) ON CREATE SET {action}"
                ));
                let error = db
                    .execute_graph_write_chain_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &denied,
                        policy(2).with_mutation_limit(10),
                        || NOW,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(authorization(&error), Some(Error::ScopeDenied), "{error:?}");
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}
