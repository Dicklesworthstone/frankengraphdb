//! Scalar and matrix batches enter the real native write facade. Expected
//! stored effects are specified independently of the binder's generated text.
//! Every successful batch has one native completion, never one per item.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, QueryWriteError, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::unwind_write::{GraphUnwindRowError, GraphUnwindWriteError};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteIdentityRequest,
    GraphWriteProgramPolicy, GraphWriteScriptExecutionError,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const ENTITY: LabelId = LabelId(1);
const ID: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const NAME: PropertyKeyId = PropertyKeyId(3);
const UPSERT: &str = "UNWIND $rows AS x MERGE (n:Entity {id:x}) \
    ON CREATE SET n.p=1 ON MATCH SET n.p=n.p+1";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(ENTITY)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        _ => None,
    }
}
fn policy(effects: u64, vertices: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000),
        effects,
        vertices,
        0,
    )
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn object(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(
        entries
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect(),
    )
    .unwrap()
}
fn args(values: Vec<GraphValue>) -> GqlParameters {
    GqlParameters::new().with_list("rows", values).unwrap()
}
fn no_allocation(_: GraphWriteIdentityRequest) -> Result<ElementId, Infallible> {
    panic!("this MATCH mutation cannot allocate a graph identity")
}
fn staged(result: QueryResult, statements: usize) {
    let QueryResult::Write {
        receipt,
        completion,
    } = result
    else {
        panic!("no-RETURN write must keep its ordinary program receipt")
    };
    assert!(completion.is_none(), "staging is not durability");
    assert_eq!(receipt.stats().completed_statements, statements);
}

#[test]
fn rekey_and_label_feedback_freeze_while_cascades_refuse_and_disjoint_updates_commit() {
    let ((), report) = run_async_under_lab(0x7363_6122, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=2 {
            seed.create_vertex(
                VId(id),
                vec![ENTITY],
                vec![
                    (ID, CanonicalScalar::Int(id as i64)),
                    (P, CanonicalScalar::Int(10)),
                ],
            );
        }
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        seed.add_edge(EId(2), VId(2), VId(1), vec![]);
        db.write(&commit, seed).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        txn.savepoint(&db, "entry").unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        for replacements in [[2, 3], [2, 1]] {
            let input = args(vec![
                object(vec![("a", int(1)), ("b", int(replacements[0]))]),
                object(vec![("a", int(2)), ("b", int(replacements[1]))]),
            ]);
            for remove_label in [false, true] {
                let tail = if remove_label {
                    "MATCH (n:Entity {id:row.a}) REMOVE n:Entity"
                } else {
                    "MATCH (n:Entity {id:row.a}) SET n.id=row.b"
                };
                staged(
                    txn.query_write(
                        &mut db,
                        &cx,
                        &format!("UNWIND $rows AS row {tail}"),
                        &input,
                        symbols,
                        R,
                        policy(20, 0),
                        no_allocation,
                    )
                    .unwrap(),
                    2,
                );
                for (index, replacement) in replacements.into_iter().enumerate() {
                    let id = index as u128 + 1;
                    let vertex = txn.vertex(&db, VId(id)).unwrap().unwrap();
                    assert_eq!(
                        vertex.labels,
                        if remove_label { vec![] } else { vec![ENTITY] }
                    );
                    assert_eq!(
                        vertex.props,
                        [
                            (
                                ID,
                                CanonicalScalar::Int(if remove_label {
                                    id as i64
                                } else {
                                    replacement
                                })
                                .into()
                            ),
                            (P, CanonicalScalar::Int(10).into()),
                        ]
                    );
                }
                txn.rollback_to_savepoint(&db, "entry").unwrap();
                assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            }
            for tail in [
                "MATCH (n:Entity {id:row.a})-[:R]->(m) DETACH DELETE n",
                "MATCH (n:Entity {id:row.a}), (anchor:Entity {id:1}) DETACH DELETE n",
            ] {
                let error = txn
                    .query_write(
                        &mut db,
                        &cx,
                        &format!("UNWIND $rows AS row {tail}"),
                        &input,
                        symbols,
                        R,
                        policy(20, 0),
                        no_allocation,
                    )
                    .unwrap_err();
                assert!(
                    matches!(
                        error,
                        QueryWriteError::UnwindBinding(GraphUnwindWriteError::RowFeedsLaterMatch {
                            statement: 0
                        })
                    ),
                    "{tail}: {error:?}"
                );
                assert_eq!(txn.staged_effect_digest().unwrap(), digest);
                assert_eq!(db.frontier().unwrap(), CommitSeq(1));
            }
        }
        for id in 1..=2 {
            let vertex = txn.vertex(&db, VId(id)).unwrap().unwrap();
            assert_eq!(vertex.labels, [ENTITY]);
            assert_eq!(
                vertex.props,
                [
                    (ID, CanonicalScalar::Int(id as i64).into()),
                    (P, CanonicalScalar::Int(10).into())
                ]
            );
        }
        assert_eq!(txn.edges(&db).unwrap().len(), 2);

        let input = args(vec![
            object(vec![("a", int(1)), ("b", int(20))]),
            object(vec![("a", int(2)), ("b", int(30))]),
        ]);
        staged(
            txn.query_write(
                &mut db,
                &cx,
                "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n.p=row.b",
                &input,
                symbols,
                R,
                policy(2, 0),
                no_allocation,
            )
            .unwrap(),
            2,
        );
        staged(
            txn.query_write(
                &mut db,
                &cx,
                "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n.id=row.b",
                &args(vec![object(vec![("a", int(1)), ("b", int(5))])]),
                symbols,
                R,
                policy(1, 0),
                no_allocation,
            )
            .unwrap(),
            1,
        );
        assert_eq!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(2)
            }
        );
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert_eq!(db.edges().unwrap().len(), 2);
        for (id, key, value) in [(1, 5, 20), (2, 2, 30)] {
            assert_eq!(
                db.vertex(VId(id)).unwrap().unwrap().props,
                [
                    (ID, CanonicalScalar::Int(key).into()),
                    (P, CanonicalScalar::Int(value).into())
                ]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compound_and_edge_filtered_disjoint_batches_preserve_values_and_commit_together() {
    let ((), report) = run_async_under_lab(0x7363_6123, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        for (id, p) in [(1, 10), (2, 0)] {
            seed.create_vertex(
                VId(id),
                vec![ENTITY],
                vec![
                    (ID, CanonicalScalar::Int(id as i64)),
                    (P, CanonicalScalar::Int(p)),
                ],
            );
        }
        for (id, source, destination, p) in [(1, 1, 2, 50), (2, 2, 1, 60)] {
            seed.add_edge(
                EId(id),
                VId(source),
                VId(destination),
                vec![
                    (ID, CanonicalScalar::Int(id as i64)),
                    (P, CanonicalScalar::Int(p)),
                ],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();

        for (tail, values) in [
            (
                "MATCH (n:Entity) WHERE n.id=row.id OR n.p=0 SET n.name='changed'",
                [(1, 0), (2, 0)],
            ),
            (
                "MATCH (n:Entity) WHERE abs(n.id)=row.id OR n.id=0 SET n.p=n.p+row.p",
                [(1, 2), (1, 3)],
            ),
            (
                "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id SET a.id=row.p",
                [(1, 11), (2, 12)],
            ),
            (
                "MATCH (a:Entity)-[e:R {id:row.id}]->(b) SET a.p=coalesce(a.p,0)+row.p",
                [(1, 100), (2, 200)],
            ),
            (
                "MATCH (a:Entity {id:row.id})-[e:R]->(b) SET e += {id:row.p}",
                [(11, 101), (12, 102)],
            ),
        ] {
            let input = args(
                values
                    .into_iter()
                    .map(|(id, p)| object(vec![("id", int(id)), ("p", int(p))]))
                    .collect(),
            );
            let frozen = input.canonical_bytes();
            staged(
                txn.query_write(
                    &mut db,
                    &cx,
                    &format!("UNWIND $rows AS row {tail}"),
                    &input,
                    symbols,
                    R,
                    policy(16, 0),
                    no_allocation,
                )
                .unwrap_or_else(|error| panic!("{tail}: {error:?}")),
                2,
            );
            assert_eq!(input.canonical_bytes(), frozen);
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        }

        // The same edge key remains unsafe when it is also the selection
        // predicate. Refusal must preserve all successful batches and prefix.
        let digest = txn.staged_effect_digest().unwrap();
        let input = args(vec![
            object(vec![("id", int(101)), ("p", int(102))]),
            object(vec![("id", int(102)), ("p", int(103))]),
        ]);
        for tail in [
            "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id OR e.p=0 SET e.id=row.p",
            "MATCH (a:Entity)-[e:R {id:row.id}]->(b) SET e += {id:row.p}",
            "MATCH (a:Entity)-[e:R]->(b) WHERE abs(e.id)=row.id SET e.id=row.p",
        ] {
            let error = txn
                .query_write(
                    &mut db,
                    &cx,
                    &format!("UNWIND $rows AS row {tail}"),
                    &input,
                    symbols,
                    R,
                    policy(16, 0),
                    no_allocation,
                )
                .unwrap_err();
            assert!(matches!(
                error,
                QueryWriteError::UnwindBinding(GraphUnwindWriteError::RowFeedsLaterMatch {
                    statement: 0
                })
            ));
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        }
        assert_eq!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(2)
            }
        );
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert!(db.vertex(VId(99)).unwrap().is_some());
        for (id, key, p) in [(1, 11, 115), (2, 12, 200)] {
            assert_eq!(
                db.vertex(VId(id)).unwrap().unwrap().props,
                [
                    (ID, CanonicalScalar::Int(key).into()),
                    (P, CanonicalScalar::Int(p).into()),
                    (
                        NAME,
                        CanonicalScalar::ucs_basic_text("changed").unwrap().into()
                    ),
                ]
            );
        }
        assert_eq!(db.edges().unwrap().len(), 2);
        for (id, key, p) in [(1, 101, 50), (2, 102, 60)] {
            assert_eq!(
                db.edge(EId(id)).unwrap().unwrap().props,
                [
                    (ID, CanonicalScalar::Int(key).into()),
                    (P, CanonicalScalar::Int(p).into()),
                ]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scalar_upserts_matrix_updates_and_correlated_tags_publish_one_durable_commit() {
    let ((), report) = run_async_under_lab(0x7363_6101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let issued = AtomicU64::new(0);
        let input = args(vec![int(2), int(1), int(2)]);
        let frozen = input.canonical_bytes();
        staged(
            txn.query_write(
                &mut db,
                &cx,
                UPSERT,
                &input,
                symbols,
                R,
                policy(10, 2),
                |request| {
                    assert!(
                        request.statement < 2,
                        "the third record reuses the first vertex"
                    );
                    assert!(matches!(
                        request.request,
                        GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    ));
                    let offset = issued.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, Infallible>(ElementId::Vertex(VId(u128::MAX - u128::from(offset))))
                },
            )
            .unwrap(),
            3,
        );
        assert_eq!(issued.load(Ordering::Relaxed), 2);
        assert_eq!(input.canonical_bytes(), frozen);
        assert_eq!(
            txn.vertex(&db, VId(u128::MAX)).unwrap().unwrap().props,
            vec![
                (ID, CanonicalScalar::Int(2).into()),
                (P, CanonicalScalar::Int(2).into())
            ]
        );
        assert_eq!(
            txn.vertex(&db, VId(u128::MAX - 1)).unwrap().unwrap().props,
            vec![
                (ID, CanonicalScalar::Int(1).into()),
                (P, CanonicalScalar::Int(1).into())
            ]
        );

        let matrix = args(vec![
            list(vec![int(1), int(1)]),
            list(vec![]),
            list(vec![int(2)]),
        ])
        .with_int64("step", 1)
        .unwrap();
        staged(
            txn.query_write(
                &mut db,
                &cx,
                "UNWIND $rows AS group UNWIND group AS x MATCH (n:Entity {id:x}) SET n.p=n.p+$step",
                &matrix,
                symbols,
                R,
                policy(3, 0),
                no_allocation,
            )
            .unwrap(),
            3,
        );
        let final_name = "second; 'quoted' $aa\nnext";
        let tags = args(vec![
            object(vec![
                ("id", int(1)),
                ("tags", list(vec![text("first"), text(final_name)])),
            ]),
            object(vec![
                ("id", int(2)),
                ("tags", list(vec![null(), text("two")])),
            ]),
        ]);
        staged(txn.query_write(&mut db, &cx,
            "UNWIND $rows AS row UNWIND row.tags AS tag MATCH (n:Entity {id:row.id}) SET n.name=tag",
            &tags, symbols, R, policy(4, 0), no_allocation).unwrap(), 4);
        assert!(
            db.vertices().unwrap().is_empty(),
            "no item publishes its own commit"
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        for (vertex, id, name) in [(u128::MAX, 2, "two"), (u128::MAX - 1, 1, final_name)] {
            assert_eq!(
                txn.vertex(&db, VId(vertex)).unwrap().unwrap().props,
                vec![
                    (ID, CanonicalScalar::Int(id).into()),
                    (P, CanonicalScalar::Int(3).into()),
                    (NAME, CanonicalScalar::ucs_basic_text(name).unwrap().into())
                ]
            );
        }
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(1)
            }
        ));
        assert_eq!(db.delta_since(CommitSeq(0)).unwrap().count(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(db.vertices().unwrap().len(), 2);
        for (vertex, id, name) in [(u128::MAX, 2, "two"), (u128::MAX - 1, 1, final_name)] {
            assert_eq!(
                db.vertex(VId(vertex)).unwrap().unwrap().props,
                vec![
                    (ID, CanonicalScalar::Int(id).into()),
                    (P, CanonicalScalar::Int(3).into()),
                    (NAME, CanonicalScalar::ucs_basic_text(name).unwrap().into())
                ]
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_late_scalar_execution_failure_restores_the_outer_prefix_and_original_record_location() {
    let ((), report) = run_async_under_lab(0x7363_6102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![ENTITY],
            vec![(ID, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(10))],
        );
        db.write(&commit, seed).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(77)));
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let query = "UNWIND /* é */ $rows AS x MATCH (n:Entity {id:$target}) SET n.p=100/x;";
        let invalid = args(vec![int(2), int(0)]).with_int64("target", 1).unwrap();
        let error = txn
            .query_write(
                &mut db,
                &cx,
                query,
                &invalid,
                symbols,
                R,
                policy(2, 0),
                no_allocation,
            )
            .unwrap_err();
        let QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location),
            ..
        }) = error
        else {
            panic!("wrong execution error: {error:?}")
        };
        assert_eq!(location.argument_set, 1);
        assert_eq!(location.statement, 0);
        assert_eq!(location.span, 0..query.find(';').unwrap());
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(
            txn.vertex(&db, VId(1)).unwrap().unwrap().props,
            vec![
                (ID, CanonicalScalar::Int(1).into()),
                (P, CanonicalScalar::Int(77).into())
            ]
        );
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (ID, CanonicalScalar::Int(1).into()),
                (P, CanonicalScalar::Int(10).into())
            ]
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(txcx.outstanding_obligations(), 0);

        // The autocommit facade likewise cannot publish its successful first
        // item when the next item fails. Its private obligation is discharged.
        let error = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                query,
                &invalid,
                symbols,
                R,
                policy(2, 0),
                no_allocation,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location), ..
        }) if location.argument_set == 1 && location.span == (0..query.find(';').unwrap()))
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (ID, CanonicalScalar::Int(1).into()),
                (P, CanonicalScalar::Int(77).into())
            ]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
        let valid = args(vec![int(4), int(5)]).with_int64("target", 1).unwrap();
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                query,
                &valid,
                symbols,
                R,
                policy(2, 0),
                no_allocation,
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            QueryResult::Write {
                completion: Some(EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: CommitSeq(3)
                }),
                ..
            }
        ));
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (ID, CanonicalScalar::Int(1).into()),
                (P, CanonicalScalar::Int(20).into())
            ]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-0s2pq: chained writes over a parameter UNWIND, the bulk ingest idiom
/// `UNWIND $rows AS row MERGE (a {id: row.a}) MERGE (b {id: row.b}) MERGE
/// (a)-[:R]->(b)`, run as one chain over the parameter's rows; a single MERGE
/// stays the batch writer's. A rerun merges onto the same graph.
#[test]
fn a_parameter_unwind_heads_a_chain_of_merges() {
    let ((), report) = run_async_under_lab(0x7363_6104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let input = args(vec![
            object(vec![("a", int(1)), ("b", int(2))]),
            object(vec![("a", int(2)), ("b", int(3))]),
        ]);
        let issued = AtomicU64::new(0);
        for _ in 0..2 {
            let mut txn = db.begin(&txcx).unwrap();
            let result = txn
                .query_write(
                    &mut db,
                    &cx,
                    "UNWIND $rows AS row MERGE (a:Entity {id: row.a}) \
                     MERGE (b:Entity {id: row.b}) MERGE (a)-[:R]->(b)",
                    &input,
                    symbols,
                    R,
                    GraphWriteProgramPolicy::new(
                        GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000),
                        100,
                        10,
                        10,
                    ),
                    |request| {
                        let next = u128::from(issued.fetch_add(1, Ordering::Relaxed)) + 1;
                        Ok::<_, Infallible>(match request.request {
                            GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                            GraphInsertRequest::Edge { .. } => {
                                ElementId::Edge(fgdb_types::EId(next))
                            }
                        })
                    },
                )
                .unwrap();
            assert!(matches!(result, QueryResult::Write { .. }));
            txn.finish(&mut db, &commit).await.unwrap();
            // Ids 1, 2 and 3 once each, and the pairs 1->2 and 2->3.
            let vertices = db.vertices().unwrap();
            assert_eq!(vertices.len(), 3);
            assert_eq!(db.edges().unwrap().len(), 2);
            let ids: Vec<_> = vertices
                .iter()
                .flat_map(|vertex| vertex.props.iter())
                .filter(|(key, _)| *key == ID)
                .map(|(_, value)| value.clone())
                .collect();
            for id in 1..=3 {
                let id: fgdb_types::CanonicalPropertyValue = CanonicalScalar::Int(id).into();
                assert!(ids.contains(&id), "vertex id {id:?}");
            }
        }
        // Five identities: three vertices and two edges, none on the rerun.
        assert_eq!(issued.load(Ordering::Relaxed), 5);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-0s2pq: a bulk upsert that returns rows, `UNWIND $rows AS row MERGE
/// (n {id: row.id}) [ON CREATE SET ...] RETURN ...`. The batch writer has no
/// RETURN, so the chain runs it over the parameter's rows, as it runs the
/// literal-list spelling. Expected rows follow openCypher clause order: each
/// row merges in turn, so the second id-2 row matches the vertex the first
/// one created, and RETURN reads every row's vertex after the statement.
#[test]
fn a_parameter_unwind_merge_returns_its_rows() {
    let ((), report) = run_async_under_lab(0x7363_6105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let input = args(vec![
            object(vec![("id", int(1)), ("name", text("a"))]),
            object(vec![("id", int(2)), ("name", text("b"))]),
            object(vec![("id", int(2)), ("name", text("c"))]),
        ]);
        let issued = AtomicU64::new(0);
        let run = async |db: &mut Database<MemVfs>, statement: &str| {
            let mut txn = db.begin(&txcx).unwrap();
            let result = txn
                .query_write(
                    db,
                    &cx,
                    statement,
                    &input,
                    symbols,
                    R,
                    policy(100, 10),
                    |request| {
                        let next = u128::from(issued.fetch_add(1, Ordering::Relaxed)) + 1;
                        Ok::<_, Infallible>(match request.request {
                            GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                            GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                        })
                    },
                )
                .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
            txn.finish(db, &commit).await.unwrap();
            let QueryResult::Rows { rows, .. } = result else {
                panic!("{statement}: a write RETURN returns rows")
            };
            rows.into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|cell| match cell {
                            QueryValue::Value(value) => value,
                            other => panic!("{statement}: {other:?}"),
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        let created = run(
            &mut db,
            "UNWIND $rows AS row MERGE (n:Entity {id: row.id}) \
             ON CREATE SET n.name = row.name RETURN n.id AS id, n.name AS name",
        )
        .await;
        assert_eq!(
            created,
            [
                vec![int(1), text("a")],
                vec![int(2), text("b")],
                vec![int(2), text("b")],
            ]
        );
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(issued.load(Ordering::Relaxed), 2);
        // A rerun matches all three rows and creates nothing.
        let counted = run(
            &mut db,
            "UNWIND $rows AS row MERGE (n:Entity {id: row.id}) RETURN count(*) AS c",
        )
        .await;
        assert_eq!(counted, [vec![int(3)]]);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(issued.load(Ordering::Relaxed), 2);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-0s2pq (TCK Unwind1 [6]): the ingest idiom that looks one vertex up
/// per row, merges another and links them, `UNWIND $rows AS row MATCH (y
/// {p: row.year}) MERGE (e {id: row.id}) MERGE (y)<-[:R]-(e) RETURN ...`.
/// The MATCH reads the UNWIND rows as a read does; the chain then merges per
/// row. Two rows name year 2000, so it gains two incoming edges, and a rerun
/// merges onto the same graph without a new identity.
#[test]
fn a_parameter_unwind_match_heads_a_chain_of_merges() {
    let ((), report) = run_async_under_lab(0x7363_6106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let rows = args(vec![
            object(vec![("year", int(2000)), ("id", int(1))]),
            object(vec![("year", int(2001)), ("id", int(2))]),
            object(vec![("year", int(2000)), ("id", int(3))]),
        ]);
        let issued = AtomicU64::new(0);
        let run = async |db: &mut Database<MemVfs>, statement: &str, input: &GqlParameters| {
            let mut txn = db.begin(&txcx).unwrap();
            let result = txn
                .query_write(
                    db,
                    &cx,
                    statement,
                    input,
                    symbols,
                    R,
                    GraphWriteProgramPolicy::new(
                        GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000),
                        100,
                        10,
                        10,
                    ),
                    |request| {
                        let next = u128::from(issued.fetch_add(1, Ordering::Relaxed)) + 1;
                        Ok::<_, Infallible>(match request.request {
                            GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                            GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                        })
                    },
                )
                .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
            txn.finish(db, &commit).await.unwrap();
            result
        };
        let values = |result: QueryResult| {
            let QueryResult::Rows { rows, .. } = result else {
                panic!("expected rows")
            };
            rows.into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|cell| match cell {
                            QueryValue::Value(value) => value,
                            other => panic!("{other:?}"),
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        run(
            &mut db,
            "CREATE (:Entity {p: 2000}), (:Entity {p: 2001})",
            &GqlParameters::new(),
        )
        .await;
        let text = "UNWIND $rows AS row MATCH (y:Entity {p: row.year}) \
                    MERGE (e:Entity {id: row.id}) MERGE (y)<-[:R]-(e) RETURN e.id AS x ORDER BY x";
        for _ in 0..2 {
            assert_eq!(
                values(run(&mut db, text, &rows).await),
                [vec![int(1)], vec![int(2)], vec![int(3)]]
            );
            // Two years, three events; e1 and e3 point at 2000, e2 at 2001.
            assert_eq!(db.vertices().unwrap().len(), 5);
            assert_eq!(db.edges().unwrap().len(), 3);
            assert_eq!(
                values(
                    db.query(
                        &cx,
                        "MATCH (e)-[:R]->(y) RETURN e.id AS event, y.p AS year ORDER BY event",
                        &GqlParameters::new(),
                        symbols,
                        GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000),
                    )
                    .unwrap()
                ),
                [
                    vec![int(1), int(2000)],
                    vec![int(2), int(2001)],
                    vec![int(3), int(2000)],
                ]
            );
        }
        // Two years, then three events and three edges; none on the rerun.
        assert_eq!(issued.load(Ordering::Relaxed), 8);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn keyed_edge_unwind_admits_only_endpoint_unions_and_pair_edges() {
    let ((), report) = run_async_under_lab(0x7363_6110, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=600_u128 {
            seed.create_vertex(
                VId(id),
                vec![ENTITY],
                vec![(ID, CanonicalScalar::Int(id as i64))],
            );
            if id > 1 {
                seed.add_edge(EId(id), VId(id - 1), VId(id), vec![]);
            }
        }
        db.write(&commit, seed).await.unwrap();
        let mut pairs = (100..110)
            .map(|id| object(vec![("a", int(id)), ("b", int(id + 100))]))
            .collect::<Vec<_>>();
        pairs.insert(1, pairs[0].clone());
        let values = args(pairs);
        let statement = "UNWIND $rows AS row \
            MATCH (a:Entity {id:row.a}), (b:Entity {id:row.b}) \
            MERGE (a)-[:R]->(b)";
        let bounded = GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(100, 100, 10_000_000, 10_000_000),
            20,
            0,
            20,
        );
        let issued = AtomicU64::new(10_000);
        let allocate = |request: GraphWriteIdentityRequest| {
            assert!(matches!(request.request, GraphInsertRequest::Edge { .. }));
            Ok::<_, Infallible>(ElementId::Edge(EId(u128::from(
                issued.fetch_add(1, Ordering::Relaxed),
            ))))
        };
        for pass in 0..2 {
            let result = db
                .query_write(
                    &txcx, &cx, &commit, statement, &values, symbols, R, bounded, allocate,
                )
                .await
                .unwrap();
            let QueryResult::Write {
                receipt,
                completion,
            } = result
            else {
                panic!("edge UNWIND keeps its ordinary native write receipt");
            };
            assert_eq!(receipt.stats().completed_statements, 11);
            assert_eq!(
                receipt.stats().created_edges,
                if pass == 0 { 10 } else { 0 }
            );
            assert_eq!(
                receipt.stats().selection.snapshot_records,
                if pass == 0 { 23 } else { 33 },
                "two indexed endpoints per row plus each existing pair edge"
            );
            if pass == 0 {
                assert!(matches!(
                    completion,
                    Some(EmbeddedTxnCompletion::WriteCommitted { .. })
                ));
            } else {
                assert!(matches!(
                    completion,
                    Some(EmbeddedTxnCompletion::ReadClosed { .. })
                ));
            }
            assert_eq!(issued.load(Ordering::Relaxed), 10_010);
        }
        let loaded = db
            .edges()
            .unwrap()
            .into_iter()
            .filter(|edge| edge.entry.eid.0 >= 10_000)
            .map(|edge| (edge.entry.src, edge.entry.dst))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            loaded,
            (100..110_u128).map(|id| (VId(id), VId(id + 100))).collect()
        );
        assert_eq!(db.edges().unwrap().len(), 609);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn malformed_scalar_matrix_and_overexpanded_sources_refuse_before_catalog_or_allocation() {
    let ((), report) = run_async_under_lab(0x7363_6103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let direct = "UNWIND $rows AS x MATCH (n:Entity) SET n.p=x";
        let matrix = "UNWIND $rows AS m UNWIND m AS x MATCH (n:Entity) SET n.p=x";
        let product = "UNWIND $rows AS m UNWIND m AS x UNWIND m AS y \
            MATCH (n:Entity {id:x}) SET n.p=y";
        let cases = [
            (
                direct,
                args(vec![
                    int(1),
                    GraphValue::Scalar(CanonicalScalar::Bool(true)),
                ]),
            ),
            (direct, args(vec![int(1), object(vec![("p", int(2))])])),
            (matrix, args(vec![list(vec![int(1)]), int(2)])),
            (product, args(vec![list((0..257).map(int).collect())])),
        ];
        for (at, (query, input)) in cases.iter().enumerate() {
            let frozen = input.canonical_bytes();
            let error = db
                .query_write(
                    &contexts.txn(),
                    &cx,
                    &commit,
                    query,
                    input,
                    |_, _: &str| panic!("input admission must precede every catalog callback"),
                    R,
                    policy(100_000, 100_000),
                    no_allocation,
                )
                .await
                .unwrap_err();
            match (at, error) {
                (
                    0,
                    QueryWriteError::UnwindBinding(GraphUnwindWriteError::Row {
                        row: 1,
                        kind: GraphUnwindRowError::IncompatibleFieldTypes,
                        ..
                    }),
                ) => {}
                (
                    1,
                    QueryWriteError::UnwindBinding(GraphUnwindWriteError::Row {
                        row: 1,
                        kind: GraphUnwindRowError::ExpectedScalarField,
                        ..
                    }),
                ) => {}
                (
                    2,
                    QueryWriteError::UnwindBinding(GraphUnwindWriteError::Expansion {
                        row: 1,
                        clause: 1,
                        kind: GraphUnwindRowError::ExpectedListField,
                        ..
                    }),
                ) => {}
                (
                    3,
                    QueryWriteError::UnwindBinding(GraphUnwindWriteError::TooManyRows {
                        limit: 65_536,
                        observed: 65_537,
                    }),
                ) => {}
                (_, error) => panic!("wrong admission refusal: {error:?}"),
            }
            assert_eq!(input.canonical_bytes(), frozen);
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scalar_batches_share_creation_and_effect_budgets_and_keep_issued_identities_consumed() {
    let ((), report) = run_async_under_lab(0x7363_6104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let issued = AtomicU64::new(100);
        let allocate = |_: GraphWriteIdentityRequest| {
            Ok::<_, Infallible>(ElementId::Vertex(VId(u128::from(
                issued.fetch_add(1, Ordering::Relaxed),
            ))))
        };
        let values = args(vec![int(1), int(2)]);
        let error = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                UPSERT,
                &values,
                symbols,
                R,
                policy(10, 1),
                allocate,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location), ..
        }) if location.argument_set == 1)
        );
        assert!(
            issued.load(Ordering::Relaxed) > 100,
            "first item already issued an identity"
        );
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert_eq!(txcx.outstanding_obligations(), 0);
        let first_available = issued.load(Ordering::Relaxed);
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                UPSERT,
                &values,
                symbols,
                R,
                policy(2, 2),
                allocate,
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            QueryResult::Write {
                completion: Some(EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: CommitSeq(1)
                }),
                ..
            }
        ));
        let current = db.vertices().unwrap();
        assert_eq!(current.len(), 2);
        for id in 100..first_available {
            assert!(
                db.vertex(VId(u128::from(id))).unwrap().is_none(),
                "failed program identities stay unused"
            );
        }
        let query = "UNWIND $rows AS x MATCH (n:Entity {id:x}) SET n.p=n.p+1";
        let error = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                query,
                &values,
                symbols,
                R,
                policy(1, 0),
                no_allocation,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location), ..
        }) if location.argument_set == 1)
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        let mut properties = db
            .vertices()
            .unwrap()
            .into_iter()
            .map(|row| row.props)
            .collect::<Vec<_>>();
        properties.sort();
        assert_eq!(
            properties,
            vec![
                vec![
                    (ID, CanonicalScalar::Int(1).into()),
                    (P, CanonicalScalar::Int(1).into())
                ],
                vec![
                    (ID, CanonicalScalar::Int(2).into()),
                    (P, CanonicalScalar::Int(1).into())
                ],
            ]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn large_shared_prefix_survives_nested_unwind_failure_refresh_and_savepoint_rollback() {
    let ((), report) = run_async_under_lab(0x7363_6120, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=2 {
            seed.create_vertex(
                VId(id),
                vec![ENTITY],
                vec![
                    (ID, CanonicalScalar::Int(id as i64)),
                    (P, CanonicalScalar::Int(10)),
                ],
            );
        }
        db.write(&commit, seed).await.unwrap();

        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        let payload = CanonicalScalar::ucs_basic_text(&"prefix payload ".repeat(64)).unwrap();
        for id in 1000..1512 {
            prefix.create_vertex(VId(id), vec![], vec![(NAME, payload.clone())]);
        }
        txn.write(&mut db, prefix).unwrap();
        txn.savepoint(&db, "kept").unwrap();
        txn.savepoint(&db, "same-prefix").unwrap();
        let mut unrelated = WriteBatch::new(R);
        unrelated.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(21)));
        db.write(&commit, unrelated).await.unwrap();
        let refreshed = txn.refresh_snapshot(&db, &txcx).unwrap();
        assert_eq!(refreshed, CommitSeq(2));
        let prefix_digest = txn.staged_effect_digest().unwrap();

        let issued = AtomicU64::new(0);
        staged(
            txn.query_write(
                &mut db,
                &query,
                UPSERT,
                &args(vec![int(1), int(4), int(4)]),
                symbols,
                R,
                policy(10, 1),
                |_| {
                    assert_eq!(issued.fetch_add(1, Ordering::Relaxed), 0);
                    Ok::<_, Infallible>(ElementId::Vertex(VId(9000)))
                },
            )
            .unwrap(),
            3,
        );
        assert_eq!(issued.load(Ordering::Relaxed), 1);
        assert_eq!(
            txn.vertex(&db, VId(9000)).unwrap().unwrap().props,
            vec![
                (ID, CanonicalScalar::Int(4).into()),
                (P, CanonicalScalar::Int(2).into())
            ]
        );
        let completed_digest = txn.staged_effect_digest().unwrap();
        let failure = txn
            .query_write(
                &mut db,
                &query,
                "UNWIND $rows AS x MATCH (n:Entity {id:1}) SET n.p=100/x",
                &args(vec![int(2), int(0)]),
                symbols,
                R,
                policy(2, 0),
                no_allocation,
            )
            .unwrap_err();
        assert!(matches!(
            failure,
            QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
                location: Some(location),
                ..
            }) if location.argument_set == 1
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), completed_digest);

        txn.rollback_to_savepoint(&db, "same-prefix").unwrap();
        assert_eq!(txn.basis(), refreshed);
        assert_eq!(txn.staged_effect_digest().unwrap(), prefix_digest);
        assert!(txn.vertex(&db, VId(9000)).unwrap().is_none());
        assert_eq!(
            txn.vertex(&db, VId(1)).unwrap().unwrap().props,
            vec![
                (ID, CanonicalScalar::Int(1).into()),
                (P, CanonicalScalar::Int(10).into())
            ]
        );
        assert_eq!(
            txn.vertex(&db, VId(1000)).unwrap().unwrap().props,
            vec![(NAME, payload.into())]
        );
        assert_eq!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(3)
            }
        );
        assert_eq!(db.vertices().unwrap().len(), 514);
        assert!(db.vertex(VId(9000)).unwrap().is_none());
        assert_eq!(
            db.vertex(VId(2)).unwrap().unwrap().props,
            vec![
                (ID, CanonicalScalar::Int(2).into()),
                (P, CanonicalScalar::Int(21).into())
            ]
        );
        assert_eq!(db.delta_since(refreshed).unwrap().count(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_unwind_and_savepoint_rollback_retain_noop_preparation_conflicts() {
    let ((), report) = run_async_under_lab(0x7363_6121, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        for change_observed_vertex in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            for id in 1..=2 {
                seed.create_vertex(
                    VId(id),
                    vec![ENTITY],
                    vec![
                        (ID, CanonicalScalar::Int(id as i64)),
                        (P, CanonicalScalar::Int(10)),
                    ],
                );
            }
            db.write(&commit, seed).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            for id in 1000..1128 {
                prefix.create_vertex(VId(id), vec![], vec![]);
            }
            txn.write(&mut db, prefix).unwrap();
            txn.savepoint(&db, "kept").unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            // The first row observes the before-image but normalizes to a no-op.
            // The second row refuses after the first preparation has succeeded.
            let failure = txn
                .query_write(
                    &mut db,
                    &query,
                    "UNWIND $rows AS x MATCH (n:Entity {id:1}) SET n.p=100/x",
                    &args(vec![int(10), int(0)]),
                    symbols,
                    R,
                    policy(2, 0),
                    no_allocation,
                )
                .unwrap_err();
            assert!(matches!(
                failure,
                QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
                    location: Some(location),
                    ..
                }) if location.argument_set == 1
            ));
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            txn.rollback_to_savepoint(&db, "kept").unwrap();
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_property(
                VId(if change_observed_vertex { 1 } else { 2 }),
                P,
                Some(CanonicalScalar::Int(11)),
            );
            db.write(&commit, winner).await.unwrap();
            // Do not read the transaction again before finishing: that could
            // accidentally repair an observation lost by either rollback.
            let completion = txn.finish(&mut db, &commit).await;
            if change_observed_vertex {
                assert!(matches!(
                    completion,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert!(db.vertex(VId(1000)).unwrap().is_none());
                assert_eq!(db.frontier().unwrap(), CommitSeq(2));
            } else {
                assert_eq!(
                    completion.unwrap(),
                    EmbeddedTxnCompletion::WriteCommitted {
                        commit_seq: CommitSeq(3)
                    }
                );
                assert_eq!(db.vertices().unwrap().len(), 130);
                assert_eq!(
                    db.vertex(VId(1)).unwrap().unwrap().props,
                    vec![
                        (ID, CanonicalScalar::Int(1).into()),
                        (P, CanonicalScalar::Int(10).into())
                    ]
                );
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// The edge-load idiom runs row by row (fgdb-0s2pq): each row's keyed MATCH
/// is an index lookup of its two endpoints, so over 600 vertices a batch of
/// pairs creates its relationships under a 100-record snapshot allowance that
/// one whole-table join of the vertices would exceed. Rows whose keys match
/// nothing create nothing, as in the native pipeline.
#[test]
fn a_keyed_edge_load_batch_reads_only_its_endpoints() {
    let ((), report) = run_async_under_lab(0x7363_6106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let issued = AtomicU64::new(0);
        let allocate = |request: GraphWriteIdentityRequest| {
            let next = u128::from(issued.fetch_add(1, Ordering::Relaxed)) + 1;
            Ok::<_, Infallible>(match request.request {
                GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                GraphInsertRequest::Edge { .. } => ElementId::Edge(fgdb_types::EId(next)),
            })
        };
        db.query_write(
            &contexts.txn(),
            &cx,
            &commit,
            "UNWIND range(1, 600) AS i CREATE (:Entity {id: i})",
            &GqlParameters::new(),
            symbols,
            R,
            policy(10_000, 1_000),
            allocate,
        )
        .await
        .unwrap();
        let narrow = GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(100, 10_000, 10_000_000, 10_000_000),
            100,
            0,
            10,
        );
        let pairs = args(vec![
            object(vec![("a", int(5)), ("b", int(77))]),
            object(vec![("a", int(77)), ("b", int(300))]),
            object(vec![("a", int(9_999)), ("b", int(5))]),
        ]);
        db.query_write(
            &contexts.txn(),
            &cx,
            &commit,
            "UNWIND $rows AS row MATCH (a:Entity {id: row.a}), (b:Entity {id: row.b}) \
             CREATE (a)-[:R]->(b)",
            &pairs,
            symbols,
            R,
            narrow,
            allocate,
        )
        .await
        .unwrap();
        let id_of = |vid: VId| -> fgdb_types::CanonicalPropertyValue {
            let vertex = db.vertex(vid).unwrap().unwrap();
            vertex
                .props
                .iter()
                .find(|(key, _)| *key == ID)
                .map(|(_, value)| value.clone())
                .unwrap()
        };
        let mut found: Vec<_> = db
            .edges()
            .unwrap()
            .into_iter()
            .map(|edge| (id_of(edge.entry.src), id_of(edge.entry.dst)))
            .collect();
        found.sort();
        let stored = |value: i64| -> fgdb_types::CanonicalPropertyValue {
            CanonicalScalar::Int(value).into()
        };
        assert_eq!(found, [(stored(5), stored(77)), (stored(77), stored(300))]);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
