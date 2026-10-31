//! Nested document arrays feed the production native binder and transaction
//! engine. Graph reads and hand-written expected effects are independent of
//! the parameter expander; no mock graph or alternate commit path is used.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryWriteError, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::unwind_write::{GraphUnwindRowError, GraphUnwindWriteError};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramPolicy,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use std::convert::Infallible;

const R: RelationId = RelationId(1);
const ID: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);
const SCORE: PropertyKeyId = PropertyKeyId(3);
const ENTITY: LabelId = LabelId(1);
const ROOT: LabelId = LabelId(2);
const UPSERT: &str = "UNWIND $rows AS parent UNWIND parent.children AS child \
    MERGE (n:Entity {id:child.id}) SET n.name=child.profile.name,n.score=parent.id";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x35; 32],
        DatabaseSecurityNamespaceId([0x36; 32]),
        [0x37; 32],
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
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(ENTITY)),
        (GraphSymbolKind::Label, "Root") => Some(GraphSymbol::Label(ROOT)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
        _ => None,
    }
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
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn child(id: i64, name: &str, score: i64) -> GraphValue {
    object(vec![
        ("id", int(id)),
        ("score", int(score)),
        (
            "profile",
            object(vec![(
                "name",
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text(name).unwrap()),
            )]),
        ),
    ])
}
fn parent(id: i64, children: Vec<GraphValue>) -> GraphValue {
    object(vec![("id", int(id)), ("children", list(children))])
}
fn args(rows: Vec<GraphValue>) -> GqlParameters {
    GqlParameters::new().with_list("rows", rows).unwrap()
}
fn integers(columns: &[&str], values: &[&[i64]]) -> QueryResult {
    QueryResult::Rows {
        columns: columns.iter().map(|column| (*column).to_owned()).collect(),
        rows: values
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| GraphAggregateValue::Value(int(*value)))
                    .collect()
            })
            .collect(),
    }
}

#[test]
fn nested_documents_upsert_vertices_and_correlated_edges_in_one_outer_commit() {
    let ((), report) = run_async_under_lab(0x6e65_7401, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &dir, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        for id in [100, 200] {
            seed.create_vertex(
                VId(id),
                vec![ROOT],
                vec![(ID, CanonicalScalar::Int(id as i64))],
            );
        }
        assert_eq!(db.write(&commit, seed).await.unwrap(), CommitSeq(1));
        let arguments = args(vec![
            parent(
                100,
                vec![
                    child(1, "first", 10),
                    child(1, "second", 15),
                    child(2, "two", 20),
                ],
            ),
            parent(
                200,
                vec![child(1, "latest\n'quote'; $aa", 30), child(3, "three", 40)],
            ),
        ]);
        let frozen = arguments.canonical_bytes();
        let mut txn = db.begin(&txcx).unwrap();
        let mut next = 1_000u128;
        txn.query_write(
            &mut db,
            &cx,
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
        .unwrap();
        let edges = "UNWIND $rows AS parent UNWIND parent.children AS child \
            MATCH (a:Root {id:parent.id}),(b:Entity {id:child.id}) \
            MERGE (a)-[r:R]->(b) SET r.score=child.score";
        txn.query_write(
            &mut db,
            &cx,
            edges,
            &arguments,
            symbols,
            R,
            writes(),
            |request| {
                assert!(matches!(request.request, GraphInsertRequest::Edge { .. }));
                next += 1;
                Ok::<_, Infallible>(ElementId::Edge(EId(next)))
            },
        )
        .unwrap();
        let read = "MATCH (a:Root)-[r:R]->(b:Entity) \
            RETURN a.id AS parent,b.id AS child,r.score AS score ORDER BY parent,child";
        let expected = integers(
            &["parent", "child", "score"],
            &[&[100, 1, 15], &[100, 2, 20], &[200, 1, 30], &[200, 3, 40]],
        );
        assert_eq!(
            txn.query(&db, &cx, read, &GqlParameters::new(), symbols, reads())
                .unwrap(),
            expected
        );
        assert_eq!(
            db.vertices_at(CommitSeq(1)).unwrap().len(),
            2,
            "staged children are not live"
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(arguments.canonical_bytes(), frozen);
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(2)
            }
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
        let mut properties: Vec<_> = db
            .vertices_at(CommitSeq(2))
            .unwrap()
            .into_iter()
            .filter(|row| row.props.iter().any(|(key, _)| *key == NAME))
            .map(|row| row.props)
            .collect();
        properties.sort();
        assert_eq!(
            properties,
            [
                (1, "latest\n'quote'; $aa", 200),
                (2, "two", 100),
                (3, "three", 200)
            ]
            .into_iter()
            .map(|(id, name, score)| vec![
                (ID, CanonicalScalar::Int(id).into()),
                (NAME, CanonicalScalar::ucs_basic_text(name).unwrap().into()),
                (SCORE, CanonicalScalar::Int(score).into())
            ])
            .collect::<Vec<_>>()
        );
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &dir, keys())
            .await
            .unwrap();
        assert_eq!(
            db.query(&cx, read, &GqlParameters::new(), symbols, reads())
                .unwrap(),
            expected
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn malformed_or_overexpanded_tail_cannot_resolve_allocate_or_publish_a_prefix() {
    let ((), report) = run_async_under_lab(0x6e65_7402, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let malformed = args(vec![
            parent(1, vec![child(1, "good", 1)]),
            object(vec![("id", int(2)), ("children", int(42))]),
        ]);
        // A compact input makes more than 65,536 intermediate/final bindings.
        // The public native adapter uses that batch ceiling, not bind()'s 64.
        let huge = args(vec![parent(
            1,
            (0..257).map(|id| child(id, "bounded", 1)).collect(),
        )]);
        let product = "UNWIND $rows AS parent UNWIND parent.children AS child \
            UNWIND parent.children AS sibling MERGE (n:Entity {id:child.id}) \
            SET n.name=child.profile.name,n.score=sibling.id";
        for (at, (text, arguments)) in [(UPSERT, malformed), (product, huge)].iter().enumerate() {
            let error = db
                .query_write(
                    &contexts.txn(),
                    &cx,
                    &commit,
                    text,
                    arguments,
                    |_, _: &str| panic!("no catalog access before complete input admission"),
                    R,
                    writes(),
                    |_| -> Result<ElementId, Infallible> {
                        panic!("no identity allocation before admission")
                    },
                )
                .await
                .unwrap_err();
            match (at, error) {
                (
                    0,
                    QueryWriteError::UnwindBinding(GraphUnwindWriteError::Expansion {
                        row: 1,
                        clause: 1,
                        kind: GraphUnwindRowError::ExpectedListField,
                        ..
                    }),
                ) => {}
                (
                    1,
                    QueryWriteError::UnwindBinding(GraphUnwindWriteError::TooManyRows {
                        limit: 65_536,
                        observed: 65_537,
                    }),
                ) => {}
                (_, error) => panic!("wrong admission refusal: {error:?}"),
            }
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_late_expanded_statement_failure_preserves_preceding_outer_transaction_work() {
    let ((), report) = run_async_under_lab(0x6e65_7403, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut next = 100u128;
        txn.query_write(
            &mut db,
            &cx,
            UPSERT,
            &args(vec![parent(
                9,
                vec![child(1, "one", 2), child(2, "two", 0)],
            )]),
            symbols,
            R,
            writes(),
            |_| {
                next += 1;
                Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
            },
        )
        .unwrap();
        let read = "MATCH (n:Entity) RETURN n.id AS id,n.score AS score ORDER BY id";
        let before = integers(&["id", "score"], &[&[1, 9], &[2, 9]]);
        assert_eq!(
            txn.query(&db, &cx, read, &GqlParameters::new(), symbols, reads())
                .unwrap(),
            before
        );
        let bad = "UNWIND $rows AS parent UNWIND parent.children AS child \
            MATCH (n:Entity {id:child.id}) SET n.score=100/child.score";
        let error = txn
            .query_write(
                &mut db,
                &cx,
                bad,
                &args(vec![parent(
                    9,
                    vec![child(1, "one", 2), child(2, "two", 0)],
                )]),
                symbols,
                R,
                writes(),
                |_| -> Result<ElementId, Infallible> { panic!("SET allocates no identity") },
            )
            .unwrap_err();
        assert!(
            matches!(error, QueryWriteError::Execute(_)),
            "failure must occur during execution"
        );
        assert_eq!(
            txn.query(&db, &cx, read, &GqlParameters::new(), symbols, reads())
                .unwrap(),
            before
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(1)
            }
        ));
        assert_eq!(
            db.query(&cx, read, &GqlParameters::new(), symbols, reads())
                .unwrap(),
            before
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_match_nested_lists_keep_row_aliases_duplicates_and_the_entry_overlay() {
    let ((), report) = run_async_under_lab(0x6e65_7404, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for carry in ["", "WITH p, row "] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let mut seed = WriteBatch::new(R);
            for id in 1..=4 {
                seed.create_vertex(
                    VId(id),
                    vec![ENTITY],
                    vec![(ID, CanonicalScalar::Int(id as i64))],
                );
            }
            // The graph input must see the caller's unpublished prefix.
            txn.write(&mut db, seed).unwrap();
            let arguments = args(vec![
                object(vec![
                    ("id", int(1)),
                    ("friends", list(vec![int(2), int(2)])),
                ]),
                object(vec![
                    ("id", int(2)),
                    ("friends", list(vec![int(1), int(3)])),
                ]),
                object(vec![("id", int(3)), ("friends", list(vec![]))]),
                object(vec![
                    ("id", int(4)),
                    ("friends", GraphValue::Scalar(CanonicalScalar::Null)),
                ]),
                object(vec![("id", int(3))]),
                GraphValue::Scalar(CanonicalScalar::Null),
            ]);
            let frozen = arguments.canonical_bytes();
            let statement = format!(
                "UNWIND $rows AS row MATCH (p:Entity {{id:row.id}}) \
                 {carry}UNWIND row.friends AS fid MATCH (f:Entity {{id:fid}}) \
                 MERGE (p)-[r:R]->(f) ON CREATE SET r.score=row.id \
                 ON MATCH SET r.score=r.score+10"
            );
            let mut next = 100_u128;
            txn.query_write(
                &mut db,
                &cx,
                &statement,
                &arguments,
                symbols,
                R,
                writes(),
                |request| {
                    assert!(matches!(request.request, GraphInsertRequest::Edge { .. }));
                    next += 1;
                    Ok::<_, Infallible>(ElementId::Edge(EId(next)))
                },
            )
            .unwrap();
            let read = "MATCH (p:Entity)-[r:R]->(f:Entity) \
                RETURN p.id AS source,f.id AS target,r.score AS score ORDER BY source,target";
            let expected = integers(
                &["source", "target", "score"],
                &[&[1, 2, 11], &[2, 1, 2], &[2, 3, 2]],
            );
            assert_eq!(
                txn.query(&db, &cx, read, &GqlParameters::new(), symbols, reads())
                    .unwrap(),
                expected,
                "{statement}"
            );
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(arguments.canonical_bytes(), frozen);
            assert_eq!(
                txn.finish(&mut db, &commit).await.unwrap(),
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: CommitSeq(1)
                }
            );
            assert_eq!(
                db.query(&cx, read, &GqlParameters::new(), symbols, reads())
                    .unwrap(),
                expected
            );
            let updates = args(
                [10, 20]
                    .into_iter()
                    .map(|score| {
                        object(vec![
                            ("id", int(1)),
                            ("friends", list(vec![int(2)])),
                            ("score", int(score)),
                        ])
                    })
                    .collect(),
            );
            let update_statement = format!(
                "UNWIND $rows AS row MATCH (p:Entity {{id:row.id}}) \
                 {carry}UNWIND row.friends AS fid MATCH (f:Entity {{id:fid}}) \
                 SET p.id=99, f.score=row.score"
            );
            let before = db.frontier().unwrap();
            let vertices = db.vertices().unwrap();
            let error = db
                .query_write(
                    &contexts.txn(),
                    &cx,
                    &commit,
                    &update_statement,
                    &updates,
                    symbols,
                    R,
                    writes(),
                    |_| -> Result<ElementId, Infallible> { panic!("SET allocates no identity") },
                )
                .await
                .unwrap_err();
            // Simultaneous assignments to one field must agree. Keep the
            // original conflicting input as an atomic refusal, not last-wins.
            assert!(matches!(
                error,
                QueryWriteError::Execute(fgdb_gql::GraphWriteScriptExecutionError::Program(
                    fgdb_gql::GraphWriteProgramError::Program(
                        fgdb_gql::GraphMutationProgramError::Statement {
                            statement: 0,
                            source: fgdb_gql::GqlQueryError::Source(
                                fgdb_gql::GraphMutationError::ConflictingAssignment {
                                    first_row: 0,
                                    first_action: 1,
                                    row: 1,
                                    action: 1,
                                }
                            ),
                        }
                    )
                ))
            ));
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(db.vertices().unwrap(), vertices);
            let distinct_targets = args(
                [(10, 2), (20, 3)]
                    .into_iter()
                    .map(|(score, friend)| {
                        object(vec![
                            ("id", int(1)),
                            ("friends", list(vec![int(friend)])),
                            ("score", int(score)),
                        ])
                    })
                    .collect(),
            );
            db.query_write(
                &contexts.txn(),
                &cx,
                &commit,
                &update_statement,
                &distinct_targets,
                symbols,
                R,
                writes(),
                |_| -> Result<ElementId, Infallible> { panic!("SET allocates no identity") },
            )
            .await
            .unwrap();
            // Both original occurrences belong to one MATCH input cut. A
            // row-by-row rematch loses the second root after p.id changes
            // and would omit the second target's score of 20.
            assert_eq!(
                db.query(
                    &cx,
                    "MATCH (n:Entity) WHERE n.id=2 OR n.id=3 \
                     RETURN n.id AS id,n.score AS score ORDER BY id",
                    &GqlParameters::new(),
                    symbols,
                    reads(),
                )
                .unwrap(),
                integers(&["id", "score"], &[&[2, 10], &[3, 20]])
            );
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_match_nested_lists_share_effect_limits_and_roll_back_late_failures() {
    let ((), report) = run_async_under_lab(0x6e65_7405, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=3 {
            seed.create_vertex(
                VId(id),
                vec![ENTITY],
                vec![(ID, CanonicalScalar::Int(id as i64))],
            );
        }
        txn.write(&mut db, seed).unwrap();
        let arguments = args(vec![
            object(vec![
                ("id", int(1)),
                ("friends", list(vec![int(2), int(3)])),
            ]),
            object(vec![("id", int(2)), ("friends", list(vec![int(3)]))]),
        ]);
        let prefix = "UNWIND $rows AS row MATCH (p:Entity {id:row.id}) \
            UNWIND row.friends AS fid MATCH (f:Entity {id:fid})";
        let mut limited = writes();
        limited.max_created_edges = 2;
        let mut next = 100_u128;
        for (text, policy, expected) in [
            (format!("{prefix} CREATE (p)-[:R]->(f)"), limited, "limit"),
            (
                format!(
                    "{prefix} MERGE (p)-[r:R]->(f) \
                     ON CREATE SET r.score=10/(2-row.id)"
                ),
                writes(),
                "zero",
            ),
        ] {
            let error = txn
                .query_write(&mut db, &cx, &text, &arguments, symbols, R, policy, |_| {
                    next += 1;
                    Ok::<_, Infallible>(ElementId::Edge(EId(next)))
                })
                .unwrap_err();
            if expected == "limit" {
                assert!(
                    matches!(
                        error,
                        QueryWriteError::Execute(
                            fgdb_gql::GraphWriteScriptExecutionError::Program(
                                fgdb_gql::GraphWriteProgramError::CreationBudget {
                                    statement: 0,
                                    dimension:
                                        fgdb_gql::insertion::GraphInsertLimitDimension::Edges,
                                    limit: 2,
                                    observed: 3,
                                }
                            )
                        )
                    ),
                    "{text}: {error}"
                );
            } else {
                assert!(
                    error.to_string().to_ascii_lowercase().contains(expected),
                    "{text}: {error}"
                );
            }
            assert_eq!(
                txn.query(
                    &db,
                    &cx,
                    "MATCH (n:Entity) RETURN n.id AS id ORDER BY id",
                    &GqlParameters::new(),
                    symbols,
                    reads(),
                )
                .unwrap(),
                integers(&["id"], &[&[1], &[2], &[3]])
            );
            assert_eq!(
                txn.query(
                    &db,
                    &cx,
                    "MATCH ()-[r:R]->() RETURN r",
                    &GqlParameters::new(),
                    symbols,
                    reads(),
                )
                .unwrap(),
                QueryResult::Rows {
                    columns: vec!["r".into()],
                    rows: vec![],
                }
            );
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        }
        assert_eq!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(1)
            }
        );
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn chained_multipart_vertex_merges_keep_optional_rows_and_feed_later_writes() {
    let ((), report) = run_async_under_lab(0x6e65_7406, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for (source, key) in [
            ("OPTIONAL MATCH (seen:Entity {id:k})", "k"),
            ("CALL { WITH k RETURN k AS selected }", "selected"),
            ("MATCH (anchor) WITH anchor,k MATCH (anchor)", "k"),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(
                VId(99),
                vec![ENTITY],
                vec![
                    (ID, CanonicalScalar::Int(99)),
                    (SCORE, CanonicalScalar::Int(99)),
                ],
            );
            txn.write(&mut db, seed).unwrap();
            let statement = format!(
                "CREATE (anchor:Root {{id:7}}) WITH anchor UNWIND [2,1,2] AS k \
                 {source} MERGE (n:Entity {{id:{key}}}) \
                 ON CREATE SET n.score={key} ON MATCH SET n.score=n.score+10 \
                 WITH anchor,n,k CREATE (anchor)-[:R]->(n) RETURN k,n.id AS id"
            );
            let mut next = 1_000_u128;
            let returned = txn
                .query_write(
                    &mut db,
                    &cx,
                    &statement,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    writes(),
                    |request| {
                        next += 1;
                        Ok::<_, Infallible>(match request.request {
                            GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                            GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                        })
                    },
                )
                .unwrap();
            assert_eq!(
                returned,
                integers(&["k", "id"], &[&[2, 2], &[1, 1], &[2, 2]]),
                "{statement}"
            );
            assert_eq!(
                next, 1_006,
                "one anchor, two merged vertices and three occurrence edges: {statement}"
            );
            let read = "MATCH (n:Entity) RETURN n.id AS id,n.score AS score ORDER BY id";
            let expected = integers(&["id", "score"], &[&[1, 1], &[2, 12], &[99, 99]]);
            assert_eq!(
                txn.query(&db, &cx, read, &GqlParameters::new(), symbols, reads())
                    .unwrap(),
                expected,
                "{statement}"
            );
            // The empty OPTIONAL witnesses retain all three incoming rows.
            // The later CREATE also retains the duplicate merged identity.
            assert_eq!(
                txn.query(
                    &db,
                    &cx,
                    "MATCH (:Root)-[:R]->(n:Entity) RETURN n.id AS id ORDER BY id",
                    &GqlParameters::new(),
                    symbols,
                    reads(),
                )
                .unwrap(),
                integers(&["id"], &[&[1], &[2], &[2]]),
                "{statement}"
            );
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert_eq!(
                txn.finish(&mut db, &commit).await.unwrap(),
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: CommitSeq(1)
                }
            );
            assert_eq!(
                db.query(&cx, read, &GqlParameters::new(), symbols, reads())
                    .unwrap(),
                expected
            );
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn chained_optional_call_relationship_merges_see_staged_rows_and_feed_a_later_match() {
    let ((), report) = run_async_under_lab(0x6e65_7407, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut seed = WriteBatch::new(R);
        for id in [1_u128, 2] {
            seed.create_vertex(
                VId(id),
                vec![ROOT],
                vec![(ID, CanonicalScalar::Int(id as i64))],
            );
        }
        txn.write(&mut db, seed).unwrap();
        let statement = "CREATE (a:Entity {id:10}) WITH a UNWIND [1,1,2] AS k \
            OPTIONAL MATCH (b:Root {id:k}) CALL { WITH b RETURN b AS target } \
            MERGE (a)-[r:R {id:k}]->(target) \
            ON CREATE SET r.score=k ON MATCH SET r.score=r.score+10 \
            WITH a,r,k MATCH (a)-[r:R]->(b) SET b.score=99 \
            RETURN k,b.id AS id";
        let mut next = 100_u128;
        let returned = txn
            .query_write(
                &mut db,
                &cx,
                statement,
                &GqlParameters::new(),
                symbols,
                R,
                writes(),
                |request| {
                    next += 1;
                    Ok::<_, Infallible>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .unwrap();
        assert_eq!(
            returned,
            integers(&["k", "id"], &[&[1, 1], &[1, 1], &[2, 2]])
        );
        assert_eq!(
            next, 103,
            "one source vertex and two distinct relationships"
        );
        let read = "MATCH (a:Entity)-[r:R]->(b:Root) \
            RETURN a.id AS source,b.id AS target,r.score AS score,b.score AS seen \
            ORDER BY target";
        let expected = integers(
            &["source", "target", "score", "seen"],
            &[&[10, 1, 11, 99], &[10, 2, 2, 99]],
        );
        assert_eq!(
            txn.query(&db, &cx, read, &GqlParameters::new(), symbols, reads())
                .unwrap(),
            expected
        );
        assert!(db.vertices().unwrap().is_empty());
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(1)
            }
        );
        assert_eq!(
            db.query(&cx, read, &GqlParameters::new(), symbols, reads())
                .unwrap(),
            expected
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn chained_multipart_merges_restore_the_prefix_and_share_creation_limits() {
    let ((), report) = run_async_under_lab(0x6e65_7408, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut seed = WriteBatch::new(R);
        for id in [1_u128, 2] {
            seed.create_vertex(
                VId(id),
                vec![ROOT],
                vec![(ID, CanonicalScalar::Int(id as i64))],
            );
        }
        txn.write(&mut db, seed).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let mut vertices = writes();
        vertices.max_created_vertices = 2;
        let mut edges = writes();
        edges.max_created_edges = 2;
        let mut next = 100_u128;
        for (statement, policy) in [
            (
                "CREATE (a:Entity {id:10}) WITH a \
                 OPTIONAL MATCH (b:Root {id:404}) MERGE (n:Entity {id:b.id})",
                writes(),
            ),
            (
                "CREATE (a:Entity {id:10}) WITH a \
                 OPTIONAL MATCH (b:Root {id:404}) MERGE (a)-[:R]->(b)",
                writes(),
            ),
            (
                "CREATE (a:Entity {id:10}) WITH a UNWIND [1,2] AS k \
                 OPTIONAL MATCH (b:Root {id:k}) \
                 MERGE (n:Entity {id:k}) ON CREATE SET n.score=10/(2-k)",
                writes(),
            ),
            (
                "CREATE (a:Entity {id:10}) WITH a UNWIND [1,2] AS k \
                 OPTIONAL MATCH (b:Root {id:k}) \
                 MERGE (a)-[r:R]->(b) ON CREATE SET r.score=10/(2-k)",
                writes(),
            ),
            (
                "CREATE (a:Entity {id:10}) WITH a UNWIND [1,2] AS k \
                 OPTIONAL MATCH (b:Root {id:k}) MERGE (n:Entity {id:k})",
                vertices,
            ),
            (
                "CREATE (a:Entity {id:10})-[:R]->(:Entity {id:11}) WITH a \
                 UNWIND [1,2] AS k OPTIONAL MATCH (b:Root {id:k}) MERGE (a)-[:R]->(b)",
                edges,
            ),
        ] {
            let error = txn
                .query_write(
                    &mut db,
                    &cx,
                    statement,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy,
                    |request| {
                        next += 1;
                        Ok::<_, Infallible>(match request.request {
                            GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                            GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                        })
                    },
                )
                .unwrap_err();
            assert!(
                matches!(error, QueryWriteError::Chain(_)),
                "the admitted relation must reach its native runtime refusal: {statement}: {error}"
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), digest, "{statement}");
            assert_eq!(
                txn.query(
                    &db,
                    &cx,
                    "MATCH (n) RETURN n.id AS id ORDER BY id",
                    &GqlParameters::new(),
                    symbols,
                    reads(),
                )
                .unwrap(),
                integers(&["id"], &[&[1], &[2]]),
                "{statement}"
            );
            assert_eq!(
                txn.query(
                    &db,
                    &cx,
                    "MATCH ()-[r:R]->() RETURN r",
                    &GqlParameters::new(),
                    symbols,
                    reads(),
                )
                .unwrap(),
                QueryResult::Rows {
                    columns: vec!["r".into()],
                    rows: Vec::new(),
                },
                "{statement}"
            );
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        }
        assert_eq!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(1)
            }
        );
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
