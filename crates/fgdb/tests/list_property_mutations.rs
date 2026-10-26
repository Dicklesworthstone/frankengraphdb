//! List-valued mutation proposals through real database and capability owners.
//! Storage expectations are constructed directly, not by a second evaluator.

#![recursion_limit = "256"]

use asupersync::Budget;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryPolicy, GraphMutationAction, GraphMutationPolicy,
    GraphMutationValue, GraphSetValue, GraphSymbol, GraphSymbolKind, GraphSymbolResolver,
    GraphWriteIdentityRequest, GraphWriteProgramPolicy, PreparedGraphMutation,
    PreparedGraphMutationQueryText, PreparedGraphMutationText, PreparedGraphText,
    ReverseSymbolCatalog,
};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const BASE: u128 = 1_u128 << 100;
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x7a; 32]);

struct Catalog;
impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(L)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            _ => None,
        }
    }
    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_relation(R, "R");
        catalog.insert_label(L, "Item");
        catalog.insert_property(P, "p");
        catalog.insert_property(Q, "q");
        Some(catalog)
    }
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x79; 32], NS, [0x7b; 32])
}
fn int(n: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(n))
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn value(n: i64) -> GraphValue {
    list(vec![
        int(n),
        list(vec![int(n + 1), GraphValue::Scalar(CanonicalScalar::Null)]),
        list(vec![]),
    ])
}
fn query_policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, rows, 2_000_000, 2_000_000)
}
fn policy(rows: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(rows), 100, 100, 100)
}
fn no_ids(_: GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    panic!("SET must not allocate")
}
fn run<T>(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx) -> T) -> T {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let query = contexts.query();
    let txn = contexts.txn();
    runtime.block_on(test(&commit, &query, &txn))
}
fn fixture() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, n) in [(1_u128, 1), (2, 3)] {
        batch.create_vertex_values(
            VId(BASE + id),
            vec![L],
            vec![
                (P, value(n).to_written_property().unwrap()),
                (Q, CanonicalScalar::Int(id as i64).into()),
            ],
        );
    }
    batch.add_edge(
        EId(BASE + 11),
        VId(BASE + 1),
        VId(BASE + 2),
        vec![(Q, CanonicalScalar::Int(7))],
    );
    batch
}
fn prop(db: &Database<MemVfs>, id: u128, key: PropertyKeyId) -> Option<CanonicalPropertyValue> {
    db.vertex(VId(BASE + id))
        .unwrap()
        .unwrap()
        .props
        .iter()
        .find_map(|(found, value)| (*found == key).then(|| value.clone()))
}
fn rows(result: QueryResult) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("RETURN must produce rows")
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => panic!("expected a native value"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn public_copy_updates_return_final_lists_and_maps_and_persist_both_element_domains() {
    run(async |commit, cx, txcx| {
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = db.query_write(txcx, cx, commit,
            "MATCH (n:Item) SET n.q=n.p RETURN n AS node,n.q AS q,properties(n) AS document ORDER BY node",
            &GqlParameters::new(), Catalog, R, policy(2), no_ids,
        ).await.unwrap();
        let expected: Vec<_> = [(1_u128, 1), (2, 3)]
            .into_iter()
            .map(|(id, n)| {
                vec![
                    GraphValue::Vertex(VId(BASE + id)),
                    value(n),
                    GraphValue::map(vec![("p".into(), value(n)), ("q".into(), value(n))]).unwrap(),
                ]
            })
            .collect();
        assert_eq!(rows(result), expected);
        let result = db.query_write(txcx, cx, commit,
            "MATCH (a:Item)-[r:R]->(b:Item) SET a.q=b.p,r.q=a.p RETURN a.q AS vertex_value,r.q AS edge_value",
            &GqlParameters::new(), Catalog, R, policy(1), no_ids,
        ).await.unwrap();
        assert_eq!(rows(result), vec![vec![value(3), value(1)]]);
        db.compact(commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            prop(&db, 1, Q),
            Some(value(3).to_written_property().unwrap())
        );
        assert_eq!(
            prop(&db, 2, Q),
            Some(value(3).to_written_property().unwrap())
        );
        assert_eq!(
            db.edge(EId(BASE + 11)).unwrap().unwrap().props,
            vec![(Q, value(1).to_written_property().unwrap())]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn prepared_list_expressions_use_the_real_frozen_selection_and_affected_id_receipt() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let selection =
            PreparedGraphText::prepare_with_resolver("MATCH (n:Item) RETURN n,n.p AS p", Catalog)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        let mutation = PreparedGraphMutation::prepare(
            selection,
            R,
            vec![GraphMutationAction::SetProperty {
                target: 0,
                key: Q,
                value: GraphMutationValue::Composite(GraphSetValue::List(vec![
                    GraphSetValue::Column(1),
                    GraphSetValue::Value(int(9)),
                ])),
            }],
        )
        .unwrap();
        let mut tx = db.begin(txcx).unwrap();
        let (stats, vertices, edges) = tx
            .execute_graph_mutation_returning_governed(
                &mut db,
                cx,
                &mutation,
                GraphMutationPolicy::new(query_policy(2), 2),
            )
            .unwrap();
        assert_eq!(stats.effects, 2);
        assert_eq!(vertices, vec![VId(BASE + 1), VId(BASE + 2)]);
        assert!(edges.is_empty());
        tx.commit(&mut db, commit).await.unwrap();
        for (id, n) in [(1, 1), (2, 3)] {
            assert_eq!(
                prop(&db, id, Q),
                Some(list(vec![value(n), int(9)]).to_written_property().unwrap())
            );
        }
    });
}

#[test]
fn late_invalid_property_copy_restores_its_statement_without_discarding_prior_staging() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let mut bad = WriteBatch::new(R);
        bad.set_vertex_property_value(
            VId(BASE + 2),
            P,
            Some(
                GraphValue::map(vec![("stored-map".into(), int(1))])
                    .unwrap()
                    .to_property_value()
                    .unwrap(),
            ),
        );
        db.write(commit, bad).await.unwrap();
        let mut tx = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(BASE + 1), Q, Some(CanonicalScalar::Int(101)));
        tx.write(&mut db, prefix).unwrap();
        let result = tx.query_write(
            &mut db,
            cx,
            "MATCH (n:Item) SET n.q=n.p RETURN n.q AS q LIMIT 0",
            &GqlParameters::new(),
            Catalog,
            R,
            policy(0),
            no_ids,
        );
        assert!(result.is_err());
        tx.commit(&mut db, commit).await.unwrap();
        assert_eq!(prop(&db, 1, Q), Some(CanonicalScalar::Int(101).into()));
        assert_eq!(prop(&db, 2, Q), Some(CanonicalScalar::Int(2).into()));
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property_value(
            VId(BASE + 2),
            P,
            Some(value(3).to_written_property().unwrap()),
        );
        db.write(commit, repair).await.unwrap();
        let result = db
            .query_write(
                txcx,
                cx,
                commit,
                "MATCH (n:Item) SET n.q=n.p RETURN n.q AS q LIMIT 0",
                &GqlParameters::new(),
                Catalog,
                R,
                policy(0),
                no_ids,
            )
            .await
            .unwrap();
        assert!(rows(result).is_empty());
        assert_eq!(
            prop(&db, 1, Q),
            Some(value(1).to_written_property().unwrap())
        );
        assert_eq!(
            prop(&db, 2, Q),
            Some(value(3).to_written_property().unwrap())
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn list_intents_keep_capability_checks_including_forbidden_equal_to_current_writes() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let authority =
            Authority::new(AuthKey::from_seed(0x7a11), NS, "graph", SchemaEpoch(1), 1).unwrap();
        let grant = |properties| Grant {
            branch: "main".into(),
            labels: Scope::only([L]),
            relations: Scope::only([R]),
            properties,
            rights: Rights::ReadWrite,
            limits: QueryLimits {
                max_nodes: 100_000,
                max_work: 2_000_000,
                max_rows: 100,
            },
            expires_at_ms: 10_000,
        };
        let denied = authority.issue_at(&grant(Scope::only([P])), 100).unwrap();
        let allowed = authority
            .issue_at(&grant(Scope::only([P, Q])), 100)
            .unwrap();
        let query = PreparedGraphMutationText::prepare("MATCH (n:Item) SET n.q=n.p", R, Catalog)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let before = db.frontier().unwrap();
        assert!(
            db.execute_graph_mutation_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &denied,
                "main",
                &query,
                GraphMutationPolicy::new(query_policy(100), 100),
                || 100,
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(prop(&db, 1, Q), Some(CanonicalScalar::Int(1).into()));
        let (stats, _) = db
            .execute_graph_mutation_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &allowed,
                "main",
                &query,
                GraphMutationPolicy::new(query_policy(100), 100),
                || 100,
            )
            .await
            .unwrap();
        assert_eq!(stats.effects, 2);
        assert_eq!(
            prop(&db, 1, Q),
            Some(value(1).to_written_property().unwrap())
        );
        let before = db.frontier().unwrap();
        assert!(
            db.execute_graph_mutation_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &denied,
                "main",
                &query,
                GraphMutationPolicy::new(query_policy(100), 100),
                || 100,
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

/// SIZE consumes the same masked property source as other pattern
/// predicates. A hidden List is NULL before its members can affect either
/// the answer or the signed/native work counters (FG-INV-20).
#[test]
fn authorized_size_predicates_mask_hidden_list_contents_before_observation() {
    run(async |commit, cx, txcx| {
        let authority =
            Authority::new(AuthKey::from_seed(0x7a13), NS, "graph", SchemaEpoch(1), 1).unwrap();
        let query = PreparedGraphMutationText::prepare(
            "MATCH (n:Item) WHERE size(n.p) = 3 SET n.q = 9",
            R,
            Catalog,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let outcome = async |sees_p: bool, members: usize| {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            let mut batch = fixture();
            for id in [1, 2] {
                batch.set_vertex_property_value(
                    VId(BASE + id),
                    P,
                    Some(
                        list((0..members).map(|index| int(index as i64)).collect())
                            .to_written_property()
                            .unwrap(),
                    ),
                );
            }
            db.write(commit, batch).await.unwrap();
            let grant = Grant {
                branch: "main".into(),
                labels: Scope::only([L]),
                relations: Scope::only([R]),
                properties: if sees_p {
                    Scope::only([P, Q])
                } else {
                    Scope::only([Q])
                },
                rights: Rights::ReadWrite,
                limits: QueryLimits {
                    max_nodes: 100_000,
                    max_work: 2_000_000,
                    max_rows: 100,
                },
                expires_at_ms: 10_000,
            };
            let token = authority.issue_at(&grant, 100).unwrap();
            let before = db.frontier().unwrap();
            let (stats, _) = db
                .execute_graph_mutation_authorized(
                    txcx,
                    cx,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    GraphMutationPolicy::new(query_policy(100), 100),
                    || 100,
                )
                .await
                .unwrap();
            if !sees_p {
                assert_eq!(db.frontier().unwrap(), before);
            }
            (stats, prop(&db, 1, Q), prop(&db, 2, Q))
        };
        let visible = outcome(true, 3).await;
        assert_eq!(visible.0.effects, 2);
        assert_eq!(visible.1, Some(CanonicalScalar::Int(9).into()));
        assert_eq!(visible.2, Some(CanonicalScalar::Int(9).into()));
        let hidden = outcome(false, 3).await;
        assert_eq!(hidden.0.effects, 0);
        assert_eq!(hidden.1, Some(CanonicalScalar::Int(1).into()));
        assert_eq!(hidden.2, Some(CanonicalScalar::Int(2).into()));
        // 512 members, not more: a vertex row holds at most 16 KiB, and the
        // store refuses a 4096-member list (82,021 bytes) at the setup write.
        assert_eq!(outcome(false, 512).await, hidden);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

/// An authorized write reads `x.key` of an element it computes (fgdb-694ga)
/// under the capability's property scope, exactly as its pattern reads a
/// MATCH binding's property: the unwound form writes what `SET n.q = n.p`
/// writes, under a grant that sees p and under one that does not.
#[test]
fn authorized_element_reads_mask_properties_as_pattern_reads_do() {
    run(async |commit, cx, txcx| {
        let authority =
            Authority::new(AuthKey::from_seed(0x7a12), NS, "graph", SchemaEpoch(1), 1).unwrap();
        let outcome = async |text: &str, sees_p: bool| {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            db.write(commit, fixture()).await.unwrap();
            let grant = Grant {
                branch: "main".into(),
                labels: Scope::only([L]),
                relations: Scope::only([R]),
                properties: if sees_p {
                    Scope::only([P, Q])
                } else {
                    Scope::only([Q])
                },
                rights: Rights::ReadWrite,
                limits: QueryLimits {
                    max_nodes: 100_000,
                    max_work: 2_000_000,
                    max_rows: 100,
                },
                expires_at_ms: 10_000,
            };
            let token = authority.issue_at(&grant, 100).unwrap();
            let query = PreparedGraphMutationText::prepare(text, R, Catalog)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let written = db
                .execute_graph_mutation_authorized(
                    txcx,
                    cx,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    GraphMutationPolicy::new(query_policy(100), 100),
                    || 100,
                )
                .await
                .is_ok();
            (written, prop(&db, 1, Q), prop(&db, 2, Q))
        };
        for sees_p in [true, false] {
            let direct = outcome("MATCH (n:Item) SET n.q = n.p", sees_p).await;
            let unwound = outcome(
                "MATCH (n:Item) WITH collect(n) AS ns UNWIND ns AS m SET m.q = m.p",
                sees_p,
            )
            .await;
            assert_eq!(unwound, direct, "sees p: {sees_p}");
            if sees_p {
                assert_eq!(
                    unwound,
                    (
                        true,
                        Some(value(1).to_written_property().unwrap()),
                        Some(value(3).to_written_property().unwrap())
                    )
                );
            }
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn output_allowance_failure_rolls_back_lists_and_limit_zero_does_not_limit_effects() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.frontier().unwrap();
        let result = db
            .query_write(
                txcx,
                cx,
                commit,
                "MATCH (n:Item) SET n.q=n.p RETURN n.q AS q",
                &GqlParameters::new(),
                Catalog,
                R,
                policy(1),
                no_ids,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(prop(&db, 1, Q), Some(CanonicalScalar::Int(1).into()));
        let result = db
            .query_write(
                txcx,
                cx,
                commit,
                "MATCH (n:Item) SET n.q=n.p RETURN n.q AS q LIMIT 0",
                &GqlParameters::new(),
                Catalog,
                R,
                policy(0),
                no_ids,
            )
            .await
            .unwrap();
        assert!(rows(result).is_empty());
        assert_eq!(
            prop(&db, 1, Q),
            Some(value(1).to_written_property().unwrap())
        );
        assert_eq!(
            prop(&db, 2, Q),
            Some(value(3).to_written_property().unwrap())
        );
    });
}

#[test]
fn frozen_unwind_updates_keep_list_intents_in_the_existing_ordered_writer() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        db.query_write(
            txcx,
            cx,
            commit,
            "UNWIND [1,2] AS key MATCH (n:Item {q:key}) SET n.q=n.p",
            &GqlParameters::new(),
            Catalog,
            R,
            policy(100),
            no_ids,
        )
        .await
        .unwrap();
        assert_eq!(
            prop(&db, 1, Q),
            Some(value(1).to_written_property().unwrap())
        );
        assert_eq!(
            prop(&db, 2, Q),
            Some(value(3).to_written_property().unwrap())
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn collected_elements_and_input_comprehensions_read_the_frozen_overlay() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let mut tx = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(BASE + 1), Q, Some(CanonicalScalar::Int(7)));
        tx.write(&mut db, prefix).unwrap();
        tx.query_write(
            &mut db,
            cx,
            "MATCH (n:Item) WITH collect(n) AS ns UNWIND ns AS n SET n.q = n.q * 3",
            &GqlParameters::new(),
            Catalog,
            R,
            policy(100),
            no_ids,
        )
        .unwrap();
        let result = tx.query_write(
            &mut db, cx,
            "MATCH (n:Item) WITH n, [x IN [n] | x.q] AS old SET n.q = 99 RETURN old ORDER BY old",
            &GqlParameters::new(), Catalog, R, policy(2), no_ids,
        ).unwrap();
        assert_eq!(
            rows(result),
            vec![vec![list(vec![int(6)])], vec![list(vec![int(21)])]]
        );
        // Both statements still have one private overlay; no effect is durable.
        assert_eq!(prop(&db, 1, Q), Some(CanonicalScalar::Int(1).into()));
        tx.commit(&mut db, commit).await.unwrap();
        for id in [1, 2] {
            assert_eq!(prop(&db, id, Q), Some(CanonicalScalar::Int(99).into()));
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn computed_edge_fields_and_late_failure_keep_the_whole_mutation_atomic() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        db.query_write(
            txcx,
            cx,
            commit,
            "MATCH ()-[r:R]->() WITH collect(r) AS rs UNWIND rs AS r SET r.q = r.q * 2",
            &GqlParameters::new(),
            Catalog,
            R,
            policy(100),
            no_ids,
        )
        .await
        .unwrap();
        assert_eq!(
            db.edge(EId(BASE + 11)).unwrap().unwrap().props,
            vec![(Q, CanonicalScalar::Int(14).into())]
        );
        let mut tx = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(BASE + 1), Q, Some(CanonicalScalar::Int(10)));
        tx.write(&mut db, prefix).unwrap();
        assert!(
            tx.query_write(
                &mut db,
                cx,
                "MATCH (n:Item) WITH n, [n][0] AS b SET n.q = b.q / (b.q - 2)",
                &GqlParameters::new(),
                Catalog,
                R,
                policy(100),
                no_ids,
            )
            .is_err()
        );
        tx.commit(&mut db, commit).await.unwrap();
        assert_eq!(prop(&db, 1, Q), Some(CanonicalScalar::Int(10).into()));
        assert_eq!(prop(&db, 2, Q), Some(CanonicalScalar::Int(2).into()));
    });
}

#[test]
fn authorized_computed_reads_mask_parameter_identities_keys_and_both_edge_endpoints() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let mut hidden = WriteBatch::new(R);
        hidden.create_vertex(
            VId(BASE + 3),
            vec![LabelId(2)],
            vec![(Q, CanonicalScalar::Int(43))],
        );
        hidden.add_edge(
            EId(BASE + 12),
            VId(BASE + 1),
            VId(BASE + 3),
            vec![(Q, CanonicalScalar::Int(44))],
        );
        hidden.add_edge(
            EId(BASE + 13),
            VId(BASE + 3),
            VId(BASE + 1),
            vec![(Q, CanonicalScalar::Int(45))],
        );
        db.write(commit, hidden).await.unwrap();
        let mut relation = WriteBatch::new(RelationId(2));
        relation.add_edge(
            EId(BASE + 14),
            VId(BASE + 1),
            VId(BASE + 2),
            vec![(Q, CanonicalScalar::Int(46))],
        );
        db.write(commit, relation).await.unwrap();
        let authority =
            Authority::new(AuthKey::from_seed(0x7a12), NS, "graph", SchemaEpoch(1), 1).unwrap();
        let grant = |properties| Grant {
            branch: "main".into(),
            labels: Scope::only([L]),
            relations: Scope::only([R]),
            properties,
            rights: Rights::ReadWrite,
            limits: QueryLimits {
                max_nodes: 100_000,
                max_work: 2_000_000,
                max_rows: 100,
            },
            expires_at_ms: 10_000,
        };
        let allowed = authority
            .issue_at(&grant(Scope::only([P, Q])), 100)
            .unwrap();
        let masked = authority.issue_at(&grant(Scope::only([P])), 100).unwrap();
        // The target label selects rows independently of Q, which the second
        // grant cannot read. Supplied IDs carry no authority.
        let text =
            "MATCH (t:Item) WITH t, $refs AS ns UNWIND ns AS n SET t.p = n.q RETURN 1 AS updated";
        let template = PreparedGraphMutationQueryText::prepare_with_parameter_types(
            text,
            R,
            &[("refs", GqlParameterType::List)],
            Catalog,
        )
        .unwrap();
        let mut hidden_stats = [None, None];
        for (supplied, expected) in [
            (GraphValue::Vertex(VId(BASE + 2)), Some(2)),
            (GraphValue::Edge(EId(BASE + 11)), Some(7)),
            (GraphValue::Vertex(VId(BASE + 3)), None),
            (GraphValue::Edge(EId(BASE + 12)), None),
            (GraphValue::Edge(EId(BASE + 13)), None),
            (GraphValue::Edge(EId(BASE + 14)), None),
            (GraphValue::Vertex(VId(BASE + 999)), None),
            (GraphValue::Edge(EId(BASE + 999)), None),
        ] {
            let parameters = GqlParameters::new()
                .with_list("refs", vec![supplied.clone()])
                .unwrap();
            let query = template.bind_parameters(&parameters).unwrap();
            for (masked_key, token, expected) in
                [(false, &allowed, expected), (true, &masked, None)]
            {
                let (stats, result, _) = db
                    .execute_graph_mutation_query_authorized(
                        txcx,
                        cx,
                        commit,
                        &authority,
                        token,
                        "main",
                        &query,
                        GraphMutationPolicy::new(query_policy(2), 100),
                        || 100,
                    )
                    .await
                    .unwrap();
                if !masked_key && expected.is_none() {
                    let kind = usize::from(matches!(&supplied, GraphValue::Edge(_)));
                    let observed = (stats, result.rows, result.evaluator);
                    match hidden_stats[kind] {
                        Some(prior) => assert_eq!(observed, prior, "{supplied:?}"),
                        None => hidden_stats[kind] = Some(observed),
                    }
                }
                let expected: CanonicalPropertyValue = expected
                    .map_or(CanonicalScalar::Null, CanonicalScalar::Int)
                    .into();
                assert_eq!(result.value.len(), 2, "{supplied:?}");
                for row in result.value {
                    assert_eq!(row.values(), &[int(1)], "{supplied:?}");
                }
                for target in [1, 2] {
                    assert_eq!(
                        prop(&db, target, P).unwrap_or_else(|| CanonicalScalar::Null.into()),
                        expected,
                        "{supplied:?} target {target}"
                    );
                }
            }
        }
        // Locate the signed work boundary for each public value kind, then
        // require every hidden identity to cross exactly the same boundary as
        // an absent one. Both allowed targets already store NULL, so every
        // probe is the same no-op mutation with identical visible data.
        for identities in [
            vec![
                GraphValue::Vertex(VId(BASE + 999)),
                GraphValue::Vertex(VId(BASE + 3)),
            ],
            vec![
                GraphValue::Edge(EId(BASE + 999)),
                GraphValue::Edge(EId(BASE + 12)),
                GraphValue::Edge(EId(BASE + 13)),
                GraphValue::Edge(EId(BASE + 14)),
            ],
        ] {
            let parameters = GqlParameters::new()
                .with_list("refs", vec![identities[0].clone()])
                .unwrap();
            let absent = template.bind_parameters(&parameters).unwrap();
            let (mut lower, mut upper) = (0, 100_000);
            while lower < upper {
                let middle = lower + (upper - lower) / 2;
                let mut limited = grant(Scope::only([P, Q]));
                limited.limits.max_work = middle;
                let token = authority.issue_at(&limited, 100).unwrap();
                if db
                    .execute_graph_mutation_query_authorized(
                        txcx,
                        cx,
                        commit,
                        &authority,
                        &token,
                        "main",
                        &absent,
                        GraphMutationPolicy::new(query_policy(2), 100),
                        || 100,
                    )
                    .await
                    .is_ok()
                {
                    upper = middle;
                } else {
                    lower = middle + 1;
                }
            }
            assert!(lower > 0 && lower < 100_000);
            for identity in identities {
                let parameters = GqlParameters::new()
                    .with_list("refs", vec![identity.clone()])
                    .unwrap();
                let query = template.bind_parameters(&parameters).unwrap();
                for work in [lower - 1, lower] {
                    let mut limited = grant(Scope::only([P, Q]));
                    limited.limits.max_work = work;
                    let token = authority.issue_at(&limited, 100).unwrap();
                    let result = db
                        .execute_graph_mutation_query_authorized(
                            txcx,
                            cx,
                            commit,
                            &authority,
                            &token,
                            "main",
                            &query,
                            GraphMutationPolicy::new(query_policy(2), 100),
                            || 100,
                        )
                        .await;
                    assert_eq!(
                        result.is_ok(),
                        work == lower,
                        "{identity:?} at {work}, boundary {lower}: {result:?}"
                    );
                }
            }
        }
        assert_eq!(prop(&db, 3, Q), Some(CanonicalScalar::Int(43).into()));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn authorized_chain_reads_newly_created_elements_from_the_staged_cut() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let authority =
            Authority::new(AuthKey::from_seed(0x7a13), NS, "graph", SchemaEpoch(1), 1).unwrap();
        let grant = Grant {
            branch: "main".into(),
            labels: Scope::only([L]),
            relations: Scope::only([R]),
            properties: Scope::only([P, Q]),
            rights: Rights::ReadWrite,
            limits: QueryLimits {
                max_nodes: 100_000,
                max_work: 2_000_000,
                max_rows: 100,
            },
            expires_at_ms: 10_000,
        };
        let token = authority.issue_at(&grant, 100).unwrap();
        let chain = fgdb_gql::PreparedGraphWriteChainText::prepare(
            "CREATE (n:Item {q:7}) WITH n, [n][0] AS b, [x IN [n] | x.q][0] AS old \
             SET n.p = b.q + old RETURN n.p AS p",
            R,
            Catalog,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let (_, result, _) = db
            .execute_graph_write_chain_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &token,
                "main",
                &chain,
                fgdb_gql::insertion::GraphInsertPolicy::new(query_policy(1), 10, 10),
                || 100,
            )
            .await
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(result.value[0].values(), &[int(14)]);
        let mut hidden = grant;
        hidden.properties = Scope::only([P]);
        let token = authority.issue_at(&hidden, 100).unwrap();
        let denied = fgdb_gql::PreparedGraphWriteChainText::prepare(
            "CREATE (n:Item {p:7}) WITH n, [n][0] AS b SET n.p = b.q RETURN n.p AS p",
            R,
            Catalog,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let before = db.frontier().unwrap();
        assert!(
            db.execute_graph_write_chain_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &token,
                "main",
                &denied,
                fgdb_gql::insertion::GraphInsertPolicy::new(query_policy(1), 10, 10),
                || 100,
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn owned_mutation_input_reads_carried_elements_from_the_frozen_overlay() {
    use fgdb_gql::algebra::GraphValueRow;
    use fgdb_gql::{
        GqlQueryError, GraphElementKey, GraphMutationError, GraphMutationIntent,
        GraphSetExecutionError, GraphSetProjection, GraphSetQuantifier, PreparedGraphSet,
        PreparedGraphWriteChainText,
    };

    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let mut tx = db.begin(txcx).unwrap();
        let created = VId(BASE + 3);
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(created, vec![L], vec![(Q, CanonicalScalar::Int(7))]);
        tx.write(&mut db, prefix).unwrap();

        // A CREATE's WITH projection runs before its effects are staged and is
        // not a mutation input. Compose the typed owned relation explicitly to
        // exercise its reader, without depending on insertion RETURN semantics.
        let schema = PreparedGraphWriteChainText::prepare(
            "CREATE (n:Item) SET n.p = 0 RETURN n AS carried",
            R,
            Catalog,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let anchor = PreparedGraphText::prepare_with_resolver(
            "MATCH (anchor:Item {q:2}) RETURN anchor",
            Catalog,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let input = schema
            .read_input()
            .unwrap()
            .project(
                vec![
                    GraphSetProjection::new("carried", GraphSetValue::Column(0)),
                    GraphSetProjection::new(
                        "old",
                        GraphSetValue::MapGet {
                            map: Box::new(GraphSetValue::Column(0)),
                            key: "q".into(),
                            property: GraphElementKey::Property(Q),
                        },
                    ),
                ],
                GraphSetQuantifier::All,
            )
            .unwrap()
            .cross_join(PreparedGraphSet::from(anchor))
            .unwrap();
        let mutation = PreparedGraphMutation::prepare_relation(
            input,
            R,
            vec![GraphMutationAction::SetProperty {
                target: 0,
                key: P,
                value: GraphMutationValue::Column(1),
            }],
        )
        .unwrap();
        let owned = || {
            Some(vec![GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(created),
            ])])
        };
        let allowance = GraphMutationPolicy::new(query_policy(1), 10);
        assert!(matches!(
            mutation.execute_governed_over(
                owned(),
                allowance,
                |pattern, budget| tx.execute_graph_pattern_governed(&db, cx, pattern, budget),
                || cx.checkpoint(),
            ),
            Err(GqlQueryError::Source(GraphMutationError::InputRelation(
                GraphSetExecutionError::Projection { .. }
            )))
        ));
        let mut reads = Vec::new();
        let proposal = mutation
            .execute_governed_over_reading(
                owned(),
                allowance,
                |pattern, budget| tx.execute_graph_pattern_governed(&db, cx, pattern, budget),
                |element, key| {
                    reads.push((element, key));
                    match element {
                        ElementId::Vertex(vertex) => tx.vertex_property(&db, vertex, key),
                        ElementId::Edge(edge) => tx.edge_property(&db, edge, key),
                    }
                },
                || cx.checkpoint(),
            )
            .unwrap();
        assert_eq!(reads, [(ElementId::Vertex(created), Q)]);
        assert_eq!(
            proposal.intents(),
            &[GraphMutationIntent::Property {
                vertex: created,
                key: P,
                value: Some(CanonicalScalar::Int(7).into()),
            }]
        );
        // Collection produced no effects of its own; the pre-statement create
        // remains staged, and its selected field came from that same overlay.
        assert_eq!(tx.vertex_property(&db, created, P).unwrap(), None);
        tx.commit(&mut db, commit).await.unwrap();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn computed_rhs_retains_a_read_witness_outside_the_match_root() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let parameters = GqlParameters::new()
            .with_list("refs", vec![GraphValue::Vertex(VId(BASE + 2))])
            .unwrap();
        let mut tx = db.begin(txcx).unwrap();
        tx.query_write(
            &mut db,
            cx,
            "MATCH (n:Item {q:1}) WITH n, $refs AS refs UNWIND refs AS b SET n.p = b.q",
            &parameters,
            Catalog,
            R,
            policy(100),
            no_ids,
        )
        .unwrap();
        let mut winner = WriteBatch::new(R);
        winner.set_vertex_property(VId(BASE + 2), Q, Some(CanonicalScalar::Int(3)));
        db.write(commit, winner).await.unwrap();
        assert!(matches!(
            tx.commit(&mut db, commit).await,
            Err(fgdb::WriteTxnError::Write(
                fgdb::WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }
            ))
        ));
        assert_eq!(
            prop(&db, 1, P),
            Some(value(1).to_written_property().unwrap())
        );
        assert_eq!(prop(&db, 2, Q), Some(CanonicalScalar::Int(3).into()));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}
