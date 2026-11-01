//! Row-valued relationship MERGE reuses native match groups while every
//! occurrence keeps its actions, authority checks and one atomic publication.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertStats};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteChainStep,
    PreparedGraphWriteChain, PreparedGraphWriteChainText,
};
use fgdb_types::{
    CanonicalF64, CanonicalPropertyValue, CanonicalScalar, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x74; 32]);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const HIDDEN: PropertyKeyId = PropertyKeyId(3);
const HIGH: EId = EId((1_u128 << 112) + 7);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x73; 32], NS, [0x75; 32])
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn program(text: &str, parameters: &GqlParameters) -> PreparedGraphWriteChain {
    PreparedGraphWriteChainText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(parameters)
        .expect(text)
}

fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 1_000_000),
        100,
        100,
    )
    .with_mutation_limit(1_000)
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn float(value: f64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
}

async fn seed(contexts: &PurposeContexts, parallel: bool) -> Database<MemVfs> {
    let commit = contexts.commit();
    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for id in [1, 2] {
        batch.create_vertex(VId(id), vec![L], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    for (edge, source, destination) in [(EId(3), VId(1), VId(2)), (HIGH, VId(2), VId(1))]
        .into_iter()
        .take(if parallel { 2 } else { 0 })
    {
        batch.add_edge(
            edge,
            source,
            destination,
            vec![
                (P, CanonicalScalar::Int(1)),
                (Q, CanonicalScalar::Int(0)),
                (HIDDEN, CanonicalScalar::Int(99)),
            ],
        );
    }
    db.write(&commit, batch).await.unwrap();
    db
}

async fn execute(
    contexts: &PurposeContexts,
    db: &mut Database<MemVfs>,
    authorized: bool,
    program: &PreparedGraphWriteChain,
    allowance: GraphInsertPolicy,
) -> Result<(GraphInsertStats, Vec<GraphValueRow>), String> {
    let (txn, query, commit) = (contexts.txn(), contexts.query(), contexts.commit());
    let result = if authorized {
        let issuer =
            Authority::new(AuthKey::from_seed(0x7471), NS, "graph", SchemaEpoch(1), 1).unwrap();
        let token = issuer
            .issue_at(
                &Grant {
                    branch: "main".into(),
                    labels: Scope::only([L]),
                    relations: Scope::only([R]),
                    properties: Scope::only([P, Q]),
                    rights: Rights::ReadWrite,
                    limits: QueryLimits {
                        max_nodes: 1_000_000,
                        max_work: 10_000_000,
                        max_rows: 100_000,
                    },
                    expires_at_ms: 10_000,
                },
                100,
            )
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
            || 100,
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

type EdgeValues = (VId, VId, Vec<(PropertyKeyId, CanonicalPropertyValue)>);

fn edge_values(db: &Database<MemVfs>) -> Vec<EdgeValues> {
    let mut values: Vec<_> = db
        .edges()
        .unwrap()
        .into_iter()
        .map(|edge| (edge.entry.src, edge.entry.dst, edge.props))
        .collect();
    values.sort();
    values
}

#[test]
fn repeated_row_keys_keep_every_rhs_and_exact_numeric_groups_in_both_hosts() {
    let ((), report) = run_async_under_lab(0x7471_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let large = 1_i64 << 53;
        let arguments = GqlParameters::new()
            .with_list(
                "rows",
                [
                    (int(1), 10),
                    (int(2), 20),
                    (int(1), 30),
                    (float(1.0), 40),
                    (int(large + 1), 50),
                    (float(large as f64), 60),
                ]
                .into_iter()
                .map(|(key, delta)| {
                    GraphValue::map(vec![("key".into(), key), ("delta".into(), int(delta))])
                        .unwrap()
                })
                .collect(),
            )
            .unwrap();
        let frozen = arguments.canonical_bytes();
        let text = "UNWIND $rows AS row MATCH (a:Visible {p:1}),(b:Visible {p:2})                     MERGE (a)-[e:R {p:row.key}]-(b)                     ON CREATE SET e.q=row.delta ON MATCH SET e.q=e.q+row.delta                     SET a.p=row.delta";
        for authorized in [false, true] {
            let mut reused = seed(&contexts, true).await;
            let mut ordinary = seed(&contexts, true).await;
            let stable = program(&format!("{text} RETURN row.delta,e"), &arguments);
            // A semantic no-op on the match key forces the original resolver,
            // making this a native full-resolution oracle with the same RHS.
            let fallback = program(&format!("{text},e.p=e.p RETURN row.delta,e"), &arguments);
            let mut stable_steps = 0;
            for step in stable.steps() {
                if let GraphWriteChainStep::EdgeUpsert(upsert)
                | GraphWriteChainStep::EdgeUpsertRows { upsert, .. }
                | GraphWriteChainStep::EdgeUpsertQuery { upsert, .. } = step
                {
                    assert!(upsert.preserves_match_targets());
                    stable_steps += 1;
                }
            }
            assert_eq!(stable_steps, 1);
            assert!(fallback.steps().iter().any(|step| match step {
                GraphWriteChainStep::EdgeUpsert(upsert)
                | GraphWriteChainStep::EdgeUpsertRows { upsert, .. }
                | GraphWriteChainStep::EdgeUpsertQuery { upsert, .. } =>
                    !upsert.preserves_match_targets(),
                _ => false,
            }));
            let (stats, rows) = execute(&contexts, &mut reused, authorized, &stable, policy())
                .await
                .unwrap();
            let (reference, reference_rows) =
                execute(&contexts, &mut ordinary, authorized, &fallback, policy())
                    .await
                    .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (0, 3));
            assert_eq!(stats.created_edges, reference.created_edges);
            assert!(stats.selection.snapshot_records < reference.selection.snapshot_records);
            assert_eq!(edge_values(&reused), edge_values(&ordinary));
            assert_eq!(reused.vertices().unwrap(), ordinary.vertices().unwrap());
            let occurrences: Vec<_> = rows.iter().map(|row| row.values()[0].clone()).collect();
            assert_eq!(occurrences, [10, 10, 20, 30, 30, 40, 40, 50, 60].map(int));
            assert_eq!(
                occurrences,
                reference_rows
                    .iter()
                    .map(|row| row.values()[0].clone())
                    .collect::<Vec<_>>(),
            );
            assert_eq!(rows[0].values()[1], GraphValue::Edge(EId(3)));
            assert_eq!(rows[1].values()[1], GraphValue::Edge(HIGH));
            let mut stored: Vec<_> = reused
                .edges()
                .unwrap()
                .into_iter()
                .map(|edge| {
                    let property = |key| {
                        edge.props
                            .iter()
                            .find(|(candidate, _)| *candidate == key)
                            .unwrap()
                            .1
                            .clone()
                    };
                    if edge.entry.eid == EId(3) || edge.entry.eid == HIGH {
                        assert_eq!(property(HIDDEN), CanonicalScalar::Int(99).into());
                    }
                    (property(P), property(Q))
                })
                .collect();
            stored.sort();
            let mut expected: Vec<(CanonicalPropertyValue, CanonicalPropertyValue)> = vec![
                (
                    CanonicalScalar::Int(1).into(),
                    CanonicalScalar::Int(80).into(),
                ),
                (
                    CanonicalScalar::Int(1).into(),
                    CanonicalScalar::Int(80).into(),
                ),
                (
                    CanonicalScalar::Int(2).into(),
                    CanonicalScalar::Int(20).into(),
                ),
                (
                    CanonicalScalar::Int(large + 1).into(),
                    CanonicalScalar::Int(50).into(),
                ),
                (
                    CanonicalScalar::Float(CanonicalF64::new(large as f64)).into(),
                    CanonicalScalar::Int(60).into(),
                ),
            ];
            expected.sort();
            assert_eq!(stored, expected);
            assert_eq!(arguments.canonical_bytes(), frozen);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn subsequent_key_mutation_and_new_merge_step_never_reuse_an_earlier_step() {
    let ((), report) = run_async_under_lab(0x7471_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let statement = program(
            "UNWIND [1,2,1] AS x MATCH (a:Visible {p:1}),(b:Visible {p:2})              MERGE (a)-[first:R {p:x}]->(b) SET first.q=x              WITH a,b,x,first SET first.p=first.p+10              MERGE (a)-[next:R {p:x}]->(b)              ON CREATE SET next.q=x ON MATCH SET next.q=next.q+1 RETURN next",
            &GqlParameters::new(),
        );
        for authorized in [false, true] {
            let mut db = seed(&contexts, false).await;
            let (stats, rows) = execute(&contexts, &mut db, authorized, &statement, policy())
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (0, 4));
            assert_eq!(db.edges().unwrap().len(), 4);
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0], rows[2]);
            assert_ne!(rows[0], rows[1]);
            let fresh: Vec<_> = db
                .edges()
                .unwrap()
                .into_iter()
                .filter(|edge| {
                    edge.props.iter().any(|(key, value)| {
                        *key == P
                            && (value == &CanonicalScalar::Int(1).into()
                                || value == &CanonicalScalar::Int(2).into())
                    })
                })
                .collect();
            assert_eq!(fresh.len(), 2);
            for edge in fresh {
                assert!(edge.props.contains(&(Q, CanonicalScalar::Int(2).into())));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_effect_refusal_after_reused_rows_leaves_both_hosts_unchanged() {
    let ((), report) = run_async_under_lab(0x7471_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let statement = program(
            "UNWIND [1,1,1,1] AS x MATCH (a:Visible {p:1}),(b:Visible {p:2})              MERGE (a)-[e:R {p:x}]->(b) ON MATCH SET e.q=e.q+1 RETURN e.q",
            &GqlParameters::new(),
        );
        for authorized in [false, true] {
            let mut db = seed(&contexts, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let failure = execute(
                &contexts,
                &mut db,
                authorized,
                &statement,
                policy().with_mutation_limit(2),
            )
            .await
            .unwrap_err();
            assert!(failure.contains("EffectLimit"), "{failure}");
            assert!(failure.contains("observed: 3"), "{failure}");
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
