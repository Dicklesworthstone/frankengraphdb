//! Mixed programs exercise actual staging and durability. Oracles name the
//! complete final graph and independently distinguish proposals from net effects.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphMutationProgramError, GraphSymbol, GraphSymbolKind,
    GraphWriteIdentityRequest, GraphWriteProgramError, GraphWriteProgramPolicy,
    GraphWriteStatement, PreparedGraphInsertText, PreparedGraphMutationText,
    PreparedGraphWriteProgram, PreparedGraphWriteProgramTemplate,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    EmbeddedTxnState, PurposeContexts, VId,
};
use std::cell::{Cell, RefCell};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const BASE: LabelId = LabelId(1);
const NEW: LabelId = LabelId(2);
const COPY: LabelId = LabelId(3);
const TAIL: LabelId = LabelId(4);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Label, "Base") => Some(GraphSymbol::Label(BASE)),
        (GraphSymbolKind::Label, "New") => Some(GraphSymbol::Label(NEW)),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Label, "Tail") => Some(GraphSymbol::Label(TAIL)),
        _ => None,
    }
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
        10_000,
        10_000,
        10_000,
    )
}
fn program(texts: &[&str]) -> PreparedGraphWriteProgram {
    let statements: Vec<GraphWriteStatement> = texts
        .iter()
        .map(|text| {
            // Fixture dispatch only; the public API composes explicitly typed steps.
            if text.starts_with("CREATE") || text.contains(" CREATE ") {
                PreparedGraphInsertText::prepare(text, R, symbols)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap()
                    .into()
            } else {
                PreparedGraphMutationText::prepare(text, R, symbols)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap()
                    .into()
            }
        })
        .collect();
    PreparedGraphWriteProgram::prepare(statements).unwrap()
}
fn identity(request: GraphWriteIdentityRequest) -> Result<ElementId, &'static str> {
    let at = request.statement as u128 * 10_000;
    Ok(match request.request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(1_000 + at + row as u128 * 100 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(100_000 + at + row as u128 * 100 + edge as u128))
        }
    })
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for (id, p) in [(1, 10), (2, 20)] {
        batch.create_vertex(VId(id), vec![BASE], vec![(P, CanonicalScalar::Int(p))]);
    }
    batch.add_edge(EId(11), VId(1), VId(2), vec![]);
    batch.add_edge(EId(12), VId(1), VId(2), vec![]);
    db.write(cx, batch).await.unwrap();
}
fn prefix() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(777), vec![], vec![(P, CanonicalScalar::Int(70))]);
    batch
}

#[test]
fn dependent_creations_updates_and_deletion_publish_one_exact_graph() {
    let ((), report) = run_async_under_lab(0x6d17_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let basis = db.frontier().unwrap();
        let before = db.vertices().unwrap();
        let before_edges = db.edges().unwrap();
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, prefix()).unwrap();
        let query = program(&[
            "CREATE (root:New {p:10})-[:R]->(child:New {p:20})",
            "MATCH (n:New) SET n.p=n.p+1",
            "MATCH (n:New) WHERE n.p>=20 CREATE (n)-[:R]->(copy:Copy {p:n.p*2})",
            "MATCH (n:New) WHERE n.p<20 DETACH DELETE n",
            "CREATE (tail:Tail {p:99})",
        ]);
        let frozen = query.canonical_bytes();
        let requests = RefCell::new(Vec::new());
        let stats = txn
            .execute_graph_write_program_governed(&mut db, &cx, &query, policy(), |request| {
                requests.borrow_mut().push(request);
                identity(request)
            })
            .unwrap();
        assert_eq!(
            (stats.completed_statements, stats.selection.result_rows),
            (5, 6)
        );
        assert_eq!(
            (
                stats.created_vertices,
                stats.created_edges,
                stats.mutation_effects,
                stats.target_vertex_visits
            ),
            (4, 2, 3, 3)
        );
        assert_eq!(
            stats.proposed_effects(),
            9,
            "deleting a transient creation does not refund proposals"
        );
        assert_eq!(requests.borrow().len(), 6);
        let mut expected = txn.vertices(&db).unwrap();
        let mut expected_edges = txn.edges(&db).unwrap();
        let facts: Vec<_> = expected
            .iter()
            .map(|row| (row.vid, row.labels.clone(), row.props.clone()))
            .collect();
        assert_eq!(
            facts,
            vec![
                (VId(1), vec![BASE], vec![(P, CanonicalScalar::Int(10))]),
                (VId(2), vec![BASE], vec![(P, CanonicalScalar::Int(20))]),
                (VId(777), vec![], vec![(P, CanonicalScalar::Int(70))]),
                (VId(1001), vec![NEW], vec![(P, CanonicalScalar::Int(21))]),
                (VId(21000), vec![COPY], vec![(P, CanonicalScalar::Int(42))]),
                (VId(41000), vec![TAIL], vec![(P, CanonicalScalar::Int(99))]),
            ]
        );
        let topology: Vec<_> = expected_edges
            .iter()
            .map(|row| (row.entry.eid, row.entry.src, row.entry.dst))
            .collect();
        assert_eq!(
            topology,
            vec![
                (EId(11), VId(1), VId(2)),
                (EId(12), VId(1), VId(2)),
                (EId(120000), VId(1001), VId(21000))
            ]
        );
        assert_eq!(db.vertices().unwrap(), before);
        assert_eq!(db.edges().unwrap(), before_edges);
        let EmbeddedTxnCompletion::WriteCommitted { commit_seq } =
            txn.finish(&mut db, &commit).await.unwrap()
        else {
            panic!("write completion")
        };
        assert_eq!(commit_seq.0, basis.0 + 1);
        for row in &mut expected {
            if [VId(777), VId(1001), VId(21000), VId(41000)].contains(&row.vid) {
                assert_eq!(row.created_at, basis);
                row.created_at = commit_seq;
            }
        }
        for row in &mut expected_edges {
            if row.entry.eid == EId(120000) {
                assert_eq!(row.entry.created_at, basis);
                row.entry.created_at = commit_seq;
            }
        }
        assert_eq!(db.vertices().unwrap(), expected);
        assert_eq!(db.edges().unwrap(), expected_edges);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap(), expected);
        assert_eq!(db.edges().unwrap(), expected_edges);
        assert_eq!(db.vertices_at(basis).unwrap(), before);
        assert_eq!(db.edges_at(basis).unwrap(), before_edges);
        assert_eq!(pinned.vertices().unwrap(), before);
        assert_eq!(query.canonical_bytes(), frozen);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_failures_restore_cascades_properties_and_the_prior_prefix_without_reclaiming_ids() {
    let ((), report) = run_async_under_lab(0x6d17_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for mode in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let old_edges = db.edges().unwrap();
            let last = if mode == 0 {
                "MATCH (n:Base) SET n.p=1/0"
            } else {
                "CREATE (tail:Tail {p:99})-[:R]->(other:Tail {p:100})"
            };
            let query = program(&[
                "MATCH (n:Base) WHERE n.p=10 DETACH DELETE n",
                "CREATE (x:New {p:5})-[:R]->(y:New {p:6})",
                "MATCH (n:Base) SET n.p=n.p+1",
                last,
            ]);
            let mut allowance = policy();
            if mode == 3 {
                let mut measuring = db.begin(&txcx).unwrap();
                measuring.write(&mut db, prefix()).unwrap();
                let measured = measuring
                    .execute_graph_write_program_governed(&mut db, &cx, &query, allowance, identity)
                    .unwrap();
                measuring.abort();
                allowance.mutations.query.evaluator.max_work_units =
                    measured.evaluator.work_units - 1;
            }
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, prefix()).unwrap();
            let before = txn.staged_effect_digest().unwrap();
            let requests = RefCell::new(Vec::new());
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                txn.execute_graph_write_program_governed(
                    &mut db,
                    &cx,
                    &query,
                    allowance,
                    |request| {
                        requests.borrow_mut().push(request);
                        if request.statement == 3 {
                            if mode == 1
                                && matches!(request.request, GraphInsertRequest::Edge { .. })
                            {
                                return Err("late identity refusal");
                            }
                            if mode == 2
                                && matches!(
                                    request.request,
                                    GraphInsertRequest::Vertex { vertex: 1, .. }
                                )
                            {
                                return Ok(ElementId::Vertex(VId(2)));
                            }
                            assert!(mode != 4, "injected allocator unwind");
                        }
                        identity(request)
                    },
                )
            }));
            if mode == 4 {
                assert!(outcome.is_err());
            } else {
                assert!(outcome.unwrap().is_err(), "mode {mode}");
            }
            assert!(
                requests.borrow().len() >= 3,
                "earlier CREATE issued real identities"
            );
            assert_eq!(txn.state(), EmbeddedTxnState::Active);
            assert_eq!(txn.staged_effect_digest().unwrap(), before);
            assert_eq!(txn.edges(&db).unwrap(), old_edges, "cascade must be undone");
            assert_eq!(
                txn.vertex(&db, VId(2)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(20))]
            );
            assert_eq!(txn.vertices(&db).unwrap().len(), 3);
            txn.commit(&mut db, &commit).await.unwrap();
            assert!(db.vertex(VId(777)).unwrap().is_some());
            assert_eq!(db.edges().unwrap(), old_edges);
            assert_eq!(
                db.vertices().unwrap().len(),
                3,
                "only the pre-program write commits"
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rolled_back_empty_selections_remain_conflict_bearing_without_repair_reads() {
    let ((), report) = run_async_under_lab(0x6d17_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for mode in 0..4 {
            for winner_kind in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let last = if mode == 2 {
                    "MATCH (n:Base) SET n.p=1/0"
                } else {
                    "CREATE (tail:Tail {p:1})"
                };
                let query = program(&[
                    "CREATE (x:New {p:1})",
                    "MATCH (n:Base) WHERE n.p<0 SET n.p=0",
                    last,
                ]);
                let mut allowance = policy();
                if mode == 3 {
                    let mut measuring = db.begin(&txcx).unwrap();
                    measuring.write(&mut db, prefix()).unwrap();
                    let measured = measuring
                        .execute_graph_write_program_governed(
                            &mut db, &cx, &query, allowance, identity,
                        )
                        .unwrap();
                    measuring.abort();
                    allowance.mutations.query.evaluator.max_work_units =
                        measured.evaluator.work_units - 1;
                }
                let mut txn = db.begin(&txcx).unwrap();
                txn.write(&mut db, prefix()).unwrap();
                let result = txn.execute_graph_write_program_governed(
                    &mut db,
                    &cx,
                    &query,
                    allowance,
                    |request| {
                        if mode == 1 && request.statement == 2 {
                            Err("allocator unavailable")
                        } else {
                            identity(request)
                        }
                    },
                );
                assert_eq!(result.is_ok(), mode == 0);
                // Do not inspect the transaction after execution. A subsequent
                // read must not be allowed to repair a discarded negative scan.
                let mut winner = WriteBatch::new(R);
                match winner_kind {
                    0 => {
                        winner.create_vertex(VId(888), vec![], vec![]);
                    }
                    1 => {
                        winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(-1)));
                    }
                    _ => {
                        winner.create_vertex(
                            VId(888),
                            vec![BASE],
                            vec![(P, CanonicalScalar::Int(-1))],
                        );
                    }
                }
                db.write(&commit, winner).await.unwrap();
                let result = txn.commit(&mut db, &commit).await;
                if winner_kind == 0 {
                    result.unwrap();
                    assert!(db.vertex(VId(777)).unwrap().is_some());
                } else {
                    assert!(matches!(
                        result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ));
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn preflight_fences_win_over_zero_quotas_and_allocator_side_effects() {
    let ((), report) = run_async_under_lab(0x6d17_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let query = program(&["CREATE (x:New)"]);
        let zero = GraphWriteProgramPolicy::new(GqlQueryPolicy::new(0, 0, 0, 0), 0, 0, 0);
        let calls = Cell::new(0);
        let mut txn = db.begin(&txcx).unwrap();
        let result =
            txn.execute_graph_write_program_governed(&mut foreign, &cx, &query, zero, |request| {
                calls.set(calls.get() + 1);
                identity(request)
            });
        assert!(matches!(
            result,
            Err(GraphWriteProgramError::Program(
                GraphMutationProgramError::Preflight(WriteTxnError::WrongDatabase)
            ))
        ));
        assert_eq!(txn.state(), EmbeddedTxnState::Active);
        txn.abort();
        let mut txn = db.begin(&txcx).unwrap();
        let mut other_relation = WriteBatch::new(RelationId(2));
        other_relation.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, other_relation).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let result =
            txn.execute_graph_write_program_governed(&mut db, &cx, &query, zero, |request| {
                calls.set(calls.get() + 1);
                identity(request)
            });
        assert!(matches!(
            result,
            Err(GraphWriteProgramError::Program(
                GraphMutationProgramError::Budget {
                    statement: 0,
                    dimension: fgdb_gql::GraphMutationProgramDimension::WorkUnits,
                    limit: 0,
                    observed: 1,
                }
            ))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        // A refused program must not leave its mixed-relation permission enabled.
        assert!(matches!(
            txn.write(&mut db, prefix()),
            Err(WriteTxnError::RelationMismatch { expected: RelationId(2), found: R })
        ));
        let basis = db.frontier().unwrap();
        let stats = txn
            .execute_graph_write_program_governed(&mut db, &cx, &query, policy(), identity)
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert!(txn.vertex(&db, VId(99)).unwrap().is_some());
        assert_eq!(txn.vertex(&db, VId(1000)).unwrap().unwrap().labels, vec![NEW]);
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertex(VId(99)).unwrap().is_none());
        assert!(db.vertex(VId(1000)).unwrap().is_none());
        txn.abort();
        let mut txn = db.begin(&txcx).unwrap();
        db.write(&commit, prefix()).await.unwrap();
        let result =
            txn.execute_graph_write_program_governed(&mut db, &cx, &query, zero, |request| {
                calls.set(calls.get() + 1);
                identity(request)
            });
        assert!(matches!(
            result,
            Err(GraphWriteProgramError::Program(
                GraphMutationProgramError::Preflight(WriteTxnError::SnapshotAdvanced { .. })
            ))
        ));
        txn.abort();
        let mut txn = db.begin(&txcx).unwrap();
        txn.finish(&mut db, &commit).await.unwrap();
        let result =
            txn.execute_graph_write_program_governed(&mut db, &cx, &query, zero, |request| {
                calls.set(calls.get() + 1);
                identity(request)
            });
        assert!(matches!(
            result,
            Err(GraphWriteProgramError::Program(
                GraphMutationProgramError::Preflight(WriteTxnError::Finished)
            ))
        ));
        assert_eq!(calls.get(), 0);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn deleting_a_transient_creation_does_not_license_identity_revival() {
    let ((), report) = run_async_under_lab(0x6d17_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, prefix()).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let query = program(&[
            "CREATE (x:New {p:1})",
            "MATCH (x:New) DETACH DELETE x",
            "CREATE (y:New {p:2})",
        ]);
        let result =
            txn.execute_graph_write_program_governed(&mut db, &cx, &query, policy(), |_| {
                Ok::<_, &'static str>(ElementId::Vertex(VId(1000)))
            });
        assert!(result.is_err());
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(1000)).unwrap().is_none());
        assert!(db.vertex(VId(777)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reusable_templates_bind_before_execution_and_compose_across_invocations() {
    let ((), report) = run_async_under_lab(0x6d17_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let template = PreparedGraphWriteProgramTemplate::prepare(vec![
            PreparedGraphInsertText::prepare("CREATE (x:New {p:$seed})", R, symbols)
                .unwrap()
                .into(),
            PreparedGraphMutationText::prepare("MATCH (x:New) SET x.p=x.p+$step", R, symbols)
                .unwrap()
                .into(),
        ])
        .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let first = template
            .bind_parameters(
                &GqlParameters::new()
                    .with_int64("seed", 5)
                    .unwrap()
                    .with_int64("step", 2)
                    .unwrap(),
            )
            .unwrap();
        let stats = txn
            .execute_graph_write_program_governed(&mut db, &cx, &first, policy(), identity)
            .unwrap();
        assert_eq!((stats.created_vertices, stats.mutation_effects), (1, 1));
        let before = txn.staged_effect_digest().unwrap();
        assert!(
            template
                .bind_parameters(&GqlParameters::new().with_int64("seed", 10).unwrap())
                .is_err()
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        let second = template
            .bind_parameters(
                &GqlParameters::new()
                    .with_int64("seed", 10)
                    .unwrap()
                    .with_int64("step", 3)
                    .unwrap(),
            )
            .unwrap();
        let stats = txn
            .execute_graph_write_program_governed(&mut db, &cx, &second, policy(), |request| {
                // Statement indices are program-local; the host adds an invocation
                // namespace rather than reusing the first program's issued IDs.
                Ok::<_, &'static str>(match identity(request)? {
                    ElementId::Vertex(id) => ElementId::Vertex(VId(id.0 + 10_000)),
                    ElementId::Edge(id) => ElementId::Edge(EId(id.0 + 10_000)),
                })
            })
            .unwrap();
        assert_eq!((stats.created_vertices, stats.mutation_effects), (1, 2));
        assert!(db.vertices().unwrap().is_empty());
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.vertex(VId(1000)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(10))]
        );
        assert_eq!(
            db.vertex(VId(11000)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(13))]
        );
        assert_eq!(db.frontier().unwrap().0, 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
