//! List-valued mutation proposals through real database and capability owners.
//! Storage expectations are constructed directly, not by a second evaluator.

use asupersync::Budget;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphMutationAction, GraphMutationPolicy, GraphMutationValue,
    GraphSetValue, GraphSymbol, GraphSymbolKind, GraphSymbolResolver, GraphWriteIdentityRequest,
    GraphWriteProgramPolicy, PreparedGraphMutation, PreparedGraphMutationText, PreparedGraphText,
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
