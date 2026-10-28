//! Document-root UNWIND through the public database facade, real transaction
//! overlay and durable recovery. Expected rows are independent of the binder.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryWriteError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::unwind_write::{GraphUnwindRowError, GraphUnwindWriteError};
use fgdb_gql::{
    GqlMapParameter, GqlParameterValue, GqlParameters, GqlQueryPolicy, GraphAggregateValue,
    GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use std::convert::Infallible;

const R: RelationId = RelationId(1);
const UPSERT: &str = "UNWIND $payload.groups[-1].members AS row \
    MERGE (n:Entity {id:row.id}) SET n.score=row.score";
const READ: &str = "MATCH (n:Entity) RETURN n.id AS id,n.score AS score ORDER BY id";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn reads() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000)
}
fn writes() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(reads(), 10_000, 10_000, 10_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
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
fn arguments(members: Vec<GraphValue>) -> GqlParameters {
    let mut arguments = GqlParameters::new();
    arguments
        .insert(
            "payload".to_owned(),
            GqlParameterValue::Map(
                GqlMapParameter::new(vec![
                    (
                        "groups".into(),
                        list(vec![
                            // This deliberately has the wrong shape. Only the
                            // selected last group's members are a source.
                            object(vec![("members", int(999))]),
                            object(vec![("members", list(members))]),
                        ]),
                    ),
                    ("unused".into(), list(vec![int(900)])),
                ])
                .unwrap(),
            ),
        )
        .unwrap();
    arguments
}
fn row(id: i64, score: i64) -> GraphValue {
    object(vec![("id", int(id)), ("score", int(score))])
}
fn expected(rows: &[[i64; 2]]) -> QueryResult {
    QueryResult::Rows {
        columns: vec!["id".to_owned(), "score".to_owned()],
        rows: rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| GraphAggregateValue::Value(int(*value)))
                    .collect()
            })
            .collect(),
    }
}

#[derive(Clone, Copy)]
struct DocumentCatalog;

impl fgdb_gql::GraphSymbolResolver for DocumentCatalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        if kind == GraphSymbolKind::Property && name == "tag" {
            Some(GraphSymbol::Property(PropertyKeyId(3)))
        } else {
            symbols(kind, name)
        }
    }

    fn reverse_catalog(&self) -> Option<fgdb_gql::ReverseSymbolCatalog> {
        let mut catalog = fgdb_gql::ReverseSymbolCatalog::new();
        catalog.insert_label(LabelId(1), "Entity");
        catalog.insert_relation(R, "R");
        for (key, name) in [(1, "id"), (2, "score"), (3, "tag")] {
            catalog.insert_property(PropertyKeyId(key), name);
        }
        Some(catalog)
    }
}

fn property_rows(columns: &[&str], rows: Vec<Vec<GraphValue>>) -> QueryResult {
    QueryResult::Rows {
        columns: columns.iter().map(|name| (*name).to_owned()).collect(),
        rows: rows
            .into_iter()
            .map(|row| row.into_iter().map(GraphAggregateValue::Value).collect())
            .collect(),
    }
}

#[test]
fn owner_no_return_computed_maps_rekey_replace_and_preserve_failed_prefixes() {
    use fgdb::WriteBatch;
    use fgdb_gql::{
        GqlQueryError, GraphMutationError, GraphMutationProgramError, GraphWriteProgramError,
        GraphWriteScriptExecutionError,
    };

    let ((), report) = run_async_under_lab(0x646f_6306, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit, txcx) = (contexts.query(), contexts.commit(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &dir, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=3 {
            seed.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![
                    (PropertyKeyId(1), CanonicalScalar::Int(id as i64)),
                    (PropertyKeyId(2), CanonicalScalar::Int(id as i64 * 10)),
                    (PropertyKeyId(3), CanonicalScalar::Int(id as i64 * 100)),
                ],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        txn.savepoint(&db, "entry").unwrap();
        let entry = txn.staged_effect_digest().unwrap();
        let patches = GqlParameters::new()
            .with_list(
                "rows",
                vec![
                    object(vec![
                        ("id", int(1)),
                        ("props", object(vec![("id", int(2))])),
                    ]),
                    object(vec![
                        ("id", int(2)),
                        ("props", object(vec![("score", int(77))])),
                    ]),
                    object(vec![("id", int(3)), ("props", object(vec![]))]),
                ],
            )
            .unwrap();
        let original = patches.canonical_bytes();
        for assignment in ["+=", "="] {
            let text = format!(
                "UNWIND $rows AS row MATCH (n:Entity {{id:row.id}}) SET n {assignment} row.props"
            );
            let result = txn
                .query_write(
                    &mut db,
                    &cx,
                    &text,
                    &patches,
                    DocumentCatalog,
                    R,
                    writes(),
                    |_| -> Result<ElementId, Infallible> {
                        panic!("map updates allocate no identity")
                    },
                )
                .unwrap();
            let QueryResult::Write {
                receipt,
                completion,
            } = result
            else {
                panic!("no-RETURN maps must produce a write receipt");
            };
            assert!(completion.is_none());
            assert_eq!(receipt.stats().completed_statements, 1);
            assert_eq!(
                receipt.stats().mutation_effects,
                if assignment == "+=" { 2 } else { 9 }
            );
            // Row 1 changes row 2's lookup key. Both maps still address their
            // original elements; an empty += map keeps fields, while = clears.
            let expected = if assignment == "+=" {
                vec![
                    vec![(1, 2), (2, 10), (3, 100)],
                    vec![(1, 2), (2, 77), (3, 200)],
                    vec![(1, 3), (2, 30), (3, 300)],
                ]
            } else {
                vec![vec![(1, 2)], vec![(2, 77)], vec![]]
            };
            for (index, fields) in expected.into_iter().enumerate() {
                let expected = fields
                    .into_iter()
                    .map(|(key, value)| (PropertyKeyId(key), CanonicalScalar::Int(value).into()))
                    .collect::<Vec<_>>();
                assert_eq!(
                    txn.vertex(&db, VId(index as u128 + 1))
                        .unwrap()
                        .unwrap()
                        .props,
                    expected
                );
            }
            assert_eq!(patches.canonical_bytes(), original);
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
            txn.rollback_to_savepoint(&db, "entry").unwrap();
            assert_eq!(txn.staged_effect_digest().unwrap(), entry);

            let invalid = GqlParameters::new()
                .with_list(
                    "rows",
                    vec![
                        object(vec![
                            ("id", int(1)),
                            ("props", object(vec![("score", int(99))])),
                        ]),
                        object(vec![("id", int(2)), ("props", list(vec![int(7)]))]),
                    ],
                )
                .unwrap();
            let error = txn
                .query_write(
                    &mut db,
                    &cx,
                    &text,
                    &invalid,
                    DocumentCatalog,
                    R,
                    writes(),
                    |_| -> Result<ElementId, Infallible> {
                        panic!("map updates allocate no identity")
                    },
                )
                .unwrap_err();
            assert!(
                matches!(
                    &error,
                    QueryWriteError::Execute(GraphWriteScriptExecutionError::Program(
                        GraphWriteProgramError::Program(GraphMutationProgramError::Statement {
                            statement: 0,
                            source: GqlQueryError::Source(GraphMutationError::InvalidPropertyMap {
                                row: 1,
                                ..
                            }),
                        })
                    ))
                ),
                "the native failure retains its input row: {error:?}"
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), entry);
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        }

        // Homogeneous direct whole rows must also avoid the public facade's
        // older map-to-literal lowering. One receipt proves native dispatch.
        let direct = GqlParameters::new()
            .with_list("rows", vec![row(1, 11), row(2, 22), row(3, 33)])
            .unwrap();
        let result = txn
            .query_write(
                &mut db,
                &cx,
                "UNWIND $rows AS row MATCH (n:Entity {id:row.id}) SET n = row",
                &direct,
                DocumentCatalog,
                R,
                writes(),
                |_| -> Result<ElementId, Infallible> {
                    panic!("replacement allocates no identity")
                },
            )
            .unwrap();
        let QueryResult::Write {
            receipt,
            completion,
        } = result
        else {
            panic!("whole-row replacement must produce a write receipt");
        };
        assert_eq!(receipt.stats().completed_statements, 1);
        assert!(completion.is_none());
        txn.finish(&mut db, &commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &dir, keys())
            .await
            .unwrap();
        for id in 1..=3 {
            assert_eq!(
                db.vertex(VId(id)).unwrap().unwrap().props,
                vec![
                    (PropertyKeyId(1), CanonicalScalar::Int(id as i64).into()),
                    (
                        PropertyKeyId(2),
                        CanonicalScalar::Int(id as i64 * 11).into()
                    ),
                ]
            );
        }
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn heterogeneous_document_map_updates_and_carried_replacement_survive_reopen() {
    let ((), report) = run_async_under_lab(0x646f_6304, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit, txcx) = (contexts.query(), contexts.commit(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &dir, keys())
            .await
            .unwrap();
        let mut next = 100_u128;
        db.query_write(
            &txcx,
            &cx,
            &commit,
            UPSERT,
            &arguments(vec![row(1, 10), row(2, 20), row(3, 30)]),
            DocumentCatalog,
            R,
            writes(),
            |_| {
                next += 1;
                Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
            },
        )
        .await
        .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let patches = arguments(vec![
            object(vec![
                ("id", int(1)),
                ("props", object(vec![("score", int(11))])),
            ]),
            object(vec![
                ("id", int(2)),
                ("props", object(vec![("tag", int(200))])),
            ]),
            object(vec![("id", int(3)), ("props", object(vec![]))]),
        ]);
        let frozen = patches.canonical_bytes();
        let result = txn
            .query_write(
                &mut db,
                &cx,
                "UNWIND $payload.groups[-1].members AS row \
                 MATCH (n:Entity {id:row.id}) SET n += row.props \
                 RETURN n.id AS id, properties(n) AS props ORDER BY id",
                &patches,
                DocumentCatalog,
                R,
                writes(),
                |_| -> Result<ElementId, Infallible> { panic!("map updates allocate no identity") },
            )
            .unwrap();
        assert_eq!(
            result,
            property_rows(
                &["id", "props"],
                vec![
                    vec![int(1), object(vec![("id", int(1)), ("score", int(11))])],
                    vec![
                        int(2),
                        object(vec![("id", int(2)), ("score", int(20)), ("tag", int(200))]),
                    ],
                    vec![int(3), object(vec![("id", int(3)), ("score", int(30))])],
                ],
            ),
        );
        assert_eq!(patches.canonical_bytes(), frozen);
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(
            db.query(&cx, READ, &GqlParameters::new(), DocumentCatalog, reads())
                .unwrap(),
            expected(&[[1, 10], [2, 20], [3, 30]]),
        );
        let mut replacement = GqlParameters::new();
        replacement
            .insert(
                "replacement".into(),
                GqlParameterValue::Map(
                    GqlMapParameter::new(vec![("id".into(), int(2)), ("score".into(), int(22))])
                        .unwrap(),
                ),
            )
            .unwrap();
        let replaced = property_rows(
            &["props"],
            vec![vec![object(vec![("id", int(2)), ("score", int(22))])]],
        );
        assert_eq!(
            txn.query_write(
                &mut db,
                &cx,
                "MATCH (n:Entity {id:2}) WITH n, $replacement AS patch \
                 SET n = patch RETURN properties(n) AS props",
                &replacement,
                DocumentCatalog,
                R,
                writes(),
                |_| -> Result<ElementId, Infallible> {
                    panic!("map replacement allocates no identity")
                },
            )
            .unwrap(),
            replaced,
        );
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(2)
            },
        ));
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &dir, keys())
            .await
            .unwrap();
        assert_eq!(
            db.query(&cx, READ, &GqlParameters::new(), DocumentCatalog, reads())
                .unwrap(),
            expected(&[[1, 11], [2, 22], [3, 30]]),
        );
        assert_eq!(
            db.query(
                &cx,
                "MATCH (n:Entity {id:2}) RETURN properties(n) AS props",
                &GqlParameters::new(),
                DocumentCatalog,
                reads(),
            )
            .unwrap(),
            replaced,
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_computed_map_type_failure_preserves_the_outer_transaction_prefix() {
    let ((), report) = run_async_under_lab(0x646f_6305, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit, txcx) = (contexts.query(), contexts.commit(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut next = 100_u128;
        txn.query_write(
            &mut db,
            &cx,
            UPSERT,
            &arguments(vec![row(1, 9), row(2, 9)]),
            DocumentCatalog,
            R,
            writes(),
            |_| {
                next += 1;
                Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
            },
        )
        .unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let invalid = arguments(vec![
            object(vec![
                ("id", int(1)),
                ("props", object(vec![("score", int(17))])),
            ]),
            object(vec![("id", int(2)), ("props", list(vec![int(7)]))]),
        ]);
        let error = txn
            .query_write(
                &mut db,
                &cx,
                "UNWIND $payload.groups[-1].members AS row \
             MATCH (n:Entity {id:row.id}) SET n += row.props \
             RETURN properties(n) AS props",
                &invalid,
                DocumentCatalog,
                R,
                writes(),
                |_| -> Result<ElementId, Infallible> { panic!("map updates allocate no identity") },
            )
            .unwrap_err();
        assert!(matches!(error, QueryWriteError::Mutation(_)), "{error:?}");
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(
            txn.query(
                &db,
                &cx,
                READ,
                &GqlParameters::new(),
                DocumentCatalog,
                reads()
            )
            .unwrap(),
            expected(&[[1, 9], [2, 9]]),
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(1)
            },
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_document_rows_upsert_in_order_and_survive_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x646f_6301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &dir, keys())
            .await
            .unwrap();
        let arguments = arguments(vec![row(1, 10), row(2, 20), row(1, 30)]);
        let frozen = arguments.canonical_bytes();
        let mut next = 100u128;
        db.query_write(
            &txcx,
            &cx,
            &commit,
            UPSERT,
            &arguments,
            symbols,
            R,
            writes(),
            |_| {
                next += 1;
                Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
            },
        )
        .await
        .unwrap();
        assert_eq!(
            next, 102,
            "a duplicate key must match the earlier row's vertex"
        );
        assert_eq!(arguments.canonical_bytes(), frozen);
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
        let wanted = expected(&[[1, 30], [2, 20]]);
        assert_eq!(
            db.query(&cx, READ, &GqlParameters::new(), symbols, reads())
                .unwrap(),
            wanted
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &dir, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(
            db.query(&cx, READ, &GqlParameters::new(), symbols, reads())
                .unwrap(),
            wanted
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn document_shape_and_late_row_type_refusals_never_enter_catalog_or_publish() {
    let ((), report) = run_async_under_lab(0x646f_6302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let wrong_root = GqlParameters::new().with_int64("payload", 42).unwrap();
        let wrong_tail = arguments(vec![
            row(1, 10),
            object(vec![
                ("id", int(2)),
                ("score", GraphValue::Scalar(CanonicalScalar::Bool(true))),
            ]),
        ]);
        let empty = arguments(vec![]);
        for (at, arguments) in [wrong_root, wrong_tail, empty].iter().enumerate() {
            let error = db
                .query_write(
                    &txcx,
                    &cx,
                    &commit,
                    UPSERT,
                    arguments,
                    |_, _: &str| panic!("complete document admission precedes the catalog"),
                    R,
                    writes(),
                    |_| -> Result<ElementId, Infallible> { panic!("no identity allocation") },
                )
                .await
                .unwrap_err();
            match (at, error) {
                (
                    0,
                    QueryWriteError::UnwindBinding(GraphUnwindWriteError::Expansion {
                        row: 0,
                        clause: 0,
                        offset,
                        kind: GraphUnwindRowError::ExpectedMapField,
                    }),
                ) => assert_eq!(offset, UPSERT.find("$payload").unwrap()),
                (
                    1,
                    QueryWriteError::UnwindBinding(GraphUnwindWriteError::Row {
                        row: 1,
                        kind: GraphUnwindRowError::IncompatibleFieldTypes,
                        ..
                    }),
                ) => {}
                (2, QueryWriteError::UnwindBinding(GraphUnwindWriteError::Empty)) => {}
                (_, error) => panic!("wrong admission refusal: {error:?}"),
            }
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_document_row_failure_rolls_back_only_that_batch_inside_outer_transaction() {
    let ((), report) = run_async_under_lab(0x646f_6303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &dir, keys())
            .await
            .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut next = 100u128;
        txn.query_write(
            &mut db,
            &cx,
            UPSERT,
            &arguments(vec![row(1, 9), row(2, 9)]),
            symbols,
            R,
            writes(),
            |_| {
                next += 1;
                Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
            },
        )
        .unwrap();
        let wanted = expected(&[[1, 9], [2, 9]]);
        // The first expanded SET can execute; the second divides by zero.
        // A parse/type refusal would not exercise statement-local rollback.
        let text = "UNWIND $payload.groups[-1].members AS row \
            MATCH (n:Entity {id:row.id}) SET n.score=100/row.score";
        let error = txn
            .query_write(
                &mut db,
                &cx,
                text,
                &arguments(vec![row(1, 2), row(2, 0)]),
                symbols,
                R,
                writes(),
                |_| -> Result<ElementId, Infallible> { panic!("SET allocates no identity") },
            )
            .unwrap_err();
        assert!(matches!(error, QueryWriteError::Execute(_)), "{error:?}");
        assert_eq!(
            txn.query(&db, &cx, READ, &GqlParameters::new(), symbols, reads())
                .unwrap(),
            wanted
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(1)
            }
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &dir, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(
            db.query(&cx, READ, &GqlParameters::new(), symbols, reads())
                .unwrap(),
            wanted
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
