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
                (
                    VId(1),
                    vec![BASE],
                    vec![(P, CanonicalScalar::Int(10).into())]
                ),
                (
                    VId(2),
                    vec![BASE],
                    vec![(P, CanonicalScalar::Int(20).into())]
                ),
                (VId(777), vec![], vec![(P, CanonicalScalar::Int(70).into())]),
                (
                    VId(1001),
                    vec![NEW],
                    vec![(P, CanonicalScalar::Int(21).into())]
                ),
                (
                    VId(21000),
                    vec![COPY],
                    vec![(P, CanonicalScalar::Int(42).into())]
                ),
                (
                    VId(41000),
                    vec![TAIL],
                    vec![(P, CanonicalScalar::Int(99).into())]
                ),
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
                vec![(P, CanonicalScalar::Int(20).into())]
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
            Err(WriteTxnError::RelationMismatch {
                expected: RelationId(2),
                found: R
            })
        ));
        let basis = db.frontier().unwrap();
        let stats = txn
            .execute_graph_write_program_governed(&mut db, &cx, &query, policy(), identity)
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert!(txn.vertex(&db, VId(99)).unwrap().is_some());
        assert_eq!(
            txn.vertex(&db, VId(1000)).unwrap().unwrap().labels,
            vec![NEW]
        );
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
            vec![(P, CanonicalScalar::Int(10).into())]
        );
        assert_eq!(
            db.vertex(VId(11000)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(13).into())]
        );
        assert_eq!(db.frontier().unwrap().0, 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

mod independent_edges {
    use super::*;
    use fgdb::{WriteTxn, WriteTxnError};
    use fgdb_delta_types::DeltaRow;
    use fgdb_gql::{
        GqlQueryError, GraphMutationProgramDimension, GraphWriteProgramReceipt,
        GraphWriteStepError, GraphWriteStepReceipt, GraphWriteStepStats,
    };
    use fgdb_types::QueryCx;
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    type Outcome = Result<
        GraphWriteProgramReceipt,
        GraphWriteProgramError<WriteTxnError, &'static str, Box<asupersync::error::Error>>,
    >;

    /// The live predecessor: the ordinary public per-step insertion method
    /// still calls WriteTxn::write and prepares its complete staged prefix.
    /// The public savepoint gives this test driver the same error rollback as
    /// the program guard, without a production flag or another evaluator.
    fn incumbent(
        txn: &mut WriteTxn,
        db: &mut Database<MemVfs>,
        cx: &QueryCx,
        query: &PreparedGraphWriteProgram,
        allowance: GraphWriteProgramPolicy,
        mut allocate: impl FnMut(GraphWriteIdentityRequest) -> Result<ElementId, &'static str>,
    ) -> Outcome {
        txn.savepoint(db, "independent-edge-oracle").unwrap();
        let result = cx.with_restriction(|| {
            let mut receipts = Vec::with_capacity(query.statements().len());
            let stats = query.execute_governed(
                allowance,
                |statement, input, remaining| {
                    let GraphWriteStatement::Insert(input) = input else {
                        panic!("the retained predecessor fixture contains only insertions");
                    };
                    txn.execute_graph_insert_returning_governed(
                        db,
                        cx,
                        input,
                        remaining.insertion_policy(),
                        |request| allocate(GraphWriteIdentityRequest { statement, request }),
                    )
                    .map(|(stats, vertices, edges)| {
                        receipts.push(GraphWriteStepReceipt::Insert { vertices, edges });
                        GraphWriteStepStats::Insert(stats)
                    })
                    .map_err(GraphWriteStepError::Insert)
                },
                || cx.checkpoint(),
            )?;
            Ok(GraphWriteProgramReceipt::new(stats, receipts))
        });
        if result.is_err() {
            txn.rollback_to_savepoint(db, "independent-edge-oracle")
                .unwrap();
        }
        txn.release_savepoint(db, "independent-edge-oracle")
            .unwrap();
        result
    }

    fn edge_identity(request: GraphWriteIdentityRequest) -> Result<ElementId, &'static str> {
        match request.request {
            GraphInsertRequest::Edge { row: 0, edge: 0 } if request.statement == 0 => {
                Ok(ElementId::Edge(EId(u128::MAX)))
            }
            GraphInsertRequest::Edge { .. } => identity(request),
            GraphInsertRequest::Vertex { .. } => panic!("edge-only insertion allocated a vertex"),
        }
    }

    fn loops(count: usize) -> PreparedGraphWriteProgram {
        program(&vec![
            "MATCH (n:Base) WHERE n.p=10 CREATE (n)-[:R {p:n.p}]->(n)";
            count
        ])
    }

    fn compare_receipts(new: &GraphWriteProgramReceipt, old: &GraphWriteProgramReceipt) {
        assert_eq!(new.steps(), old.steps());
        let mut comparable = new.stats();
        // The new physical path no longer walks irrelevant staged edge rows
        // for a vertex-only selection. Both report their actual work.
        comparable.evaluator = old.stats().evaluator;
        assert_eq!(comparable, old.stats());
    }

    #[test]
    fn independent_chunks_match_the_live_prefix_oracle_and_global_birth_order() {
        let ((), report) = run_async_under_lab(0x6d17_0101, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&c.commit(), vfs.clone(), &path, keys())
                .await
                .unwrap();
            seed(&mut db, &c.commit()).await;
            let mut extra = WriteBatch::new(R);
            extra.create_vertex(VId(3), vec![BASE], vec![(P, CanonicalScalar::Int(20))]);
            extra.create_vertex(
                VId(u128::MAX),
                vec![BASE],
                vec![(P, CanonicalScalar::Int(30))],
            );
            db.write(&c.commit(), extra).await.unwrap();
            let basis = db.frontier().unwrap();
            let saved = db.read_session().unwrap();
            let query = program(&[
                "MATCH (a:Base),(b:Base) WHERE a.p=10 AND b.p=20 CREATE (a)-[:R {p:b.p}]->(b)",
                "MATCH (n:Base) WHERE n.p=99 CREATE (n)-[:R]->(n)",
                "MATCH (n:Base) WHERE n.p=30 CREATE (n)-[:R {p:n.p}]->(n)",
                "MATCH (a:Base),(b:Base) WHERE a.p=10 AND b.p=20 CREATE (a)-[:R {p:b.p}]->(b)",
            ]);
            for step in query.statements() {
                let GraphWriteStatement::Insert(input) = step else {
                    unreachable!();
                };
                assert_eq!(input.independent_edge_append_relation(), Some(R));
            }
            let frozen = query.canonical_bytes();
            let mut warm = db.begin(&c.txn()).unwrap();
            incumbent(
                &mut warm,
                &mut db,
                &c.query(),
                &query,
                policy(),
                edge_identity,
            )
            .unwrap();
            warm.abort();
            let mut old_txn = db.begin(&c.txn()).unwrap();
            let old = incumbent(
                &mut old_txn,
                &mut db,
                &c.query(),
                &query,
                policy(),
                edge_identity,
            )
            .unwrap();
            let mut txn = db.begin(&c.txn()).unwrap();
            txn.savepoint(&db, "before").unwrap();
            let requests = RefCell::new(Vec::new());
            let new = txn
                .execute_graph_write_program_returning_governed(
                    &mut db,
                    &c.query(),
                    &query,
                    policy(),
                    |request| {
                        requests.borrow_mut().push(request);
                        edge_identity(request)
                    },
                )
                .unwrap();
            compare_receipts(&new, &old);
            assert_eq!(new.stats().created_edges, 5);
            assert!(new.stats().evaluator.work_units < old.stats().evaluator.work_units);
            assert_eq!(
                txn.staged_effect_digest().unwrap(),
                old_txn.staged_effect_digest().unwrap()
            );
            let expected = old_txn.edges(&db).unwrap();
            assert_eq!(txn.edges(&db).unwrap(), expected);
            assert_eq!(db.edges().unwrap().len(), 2);
            assert_eq!(query.canonical_bytes(), frozen);
            old_txn.abort();
            txn.rollback_to_savepoint(&db, "before").unwrap();
            assert_eq!(txn.edges(&db).unwrap().len(), 2);
            let retry = txn
                .execute_graph_write_program_returning_governed(
                    &mut db,
                    &c.query(),
                    &query,
                    policy(),
                    edge_identity,
                )
                .unwrap();
            compare_receipts(&retry, &new);
            txn.commit(&mut db, &c.commit()).await.unwrap();
            assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
            let births: BTreeMap<_, _> = db
                .delta_since(basis)
                .unwrap()
                .flat_map(|batch| batch.coordinate_entries())
                .flat_map(|coordinate| &coordinate.rows)
                .map(|row| match row {
                    DeltaRow::CreateEdge {
                        eid, birth_ordinal, ..
                    } => (*eid, *birth_ordinal),
                    _ => panic!("the edge program published another effect kind"),
                })
                .collect();
            let expected_births: BTreeMap<_, _> = requests
                .borrow()
                .iter()
                .enumerate()
                .map(|(ordinal, &request)| {
                    let ElementId::Edge(eid) = edge_identity(request).unwrap() else {
                        unreachable!();
                    };
                    (eid, ordinal as u64 + 1)
                })
                .collect();
            assert_eq!(
                births, expected_births,
                "empty steps do not renumber births"
            );
            assert!(saved.edge(EId(u128::MAX)).unwrap().is_none());
            let committed = db.edges().unwrap();
            db.compact(&c.commit()).await.unwrap();
            drop(db);
            let db = Database::open_with_vfs(&c.commit(), vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(db.edges().unwrap(), committed);
            assert_eq!(c.txn().outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn dependent_inputs_and_existing_work_keep_the_ordinary_overlay() {
        let ((), report) = run_async_under_lab(0x6d17_0102, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed(&mut db, &c.commit()).await;
            for text in [
                "MATCH (a)-[:R]->(b) CREATE (a)-[:R]->(b)",
                "MATCH (n:Base) CREATE (n)-[:R]->(new:New)",
                "UNWIND [1] AS x MATCH (n:Base) CREATE (n)-[:R]->(n)",
            ] {
                let input = PreparedGraphInsertText::prepare(text, R, symbols)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap();
                assert_eq!(input.independent_edge_append_relation(), None, "{text}");
            }
            let dependent = program(&[
                "MATCH (a)-[:R]->(b) CREATE (a)-[:R]->(b)",
                "MATCH (a)-[:R]->(b) CREATE (a)-[:R]->(b)",
            ]);
            let mut txn = db.begin(&c.txn()).unwrap();
            let actual = txn
                .execute_graph_write_program_governed(
                    &mut db,
                    &c.query(),
                    &dependent,
                    policy(),
                    identity,
                )
                .unwrap();
            assert_eq!(
                actual.created_edges,
                2 + 4,
                "the second step sees the first two edges"
            );
            assert_eq!(txn.edges(&db).unwrap().len(), 8);
            txn.abort();

            let query = loops(3);
            let mut old = db.begin(&c.txn()).unwrap();
            let mut txn = db.begin(&c.txn()).unwrap();
            let mut changed = WriteBatch::new(R);
            changed.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
            old.write(&mut db, changed.clone()).unwrap();
            txn.write(&mut db, changed).unwrap();
            let before = txn.staged_effect_digest().unwrap();
            let expected =
                incumbent(&mut old, &mut db, &c.query(), &query, policy(), identity).unwrap();
            let actual = txn
                .execute_graph_write_program_returning_governed(
                    &mut db,
                    &c.query(),
                    &query,
                    policy(),
                    identity,
                )
                .unwrap();
            assert_eq!(actual.stats(), expected.stats());
            assert_eq!(
                actual.stats().created_edges,
                0,
                "MATCH sees the staged property"
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), before);
            old.abort();
            txn.abort();

            // Each step is individually independent, but their coordinates
            // differ. The ordinary atomic path must retain both relations.
            let other = RelationId(2);
            let first = loops(2).statements()[0].clone();
            let second = PreparedGraphInsertText::prepare(
                "MATCH (n:Base) WHERE n.p=10 CREATE (n)-[:S {p:n.p}]->(n)",
                other,
                |kind: GraphSymbolKind, name: &str| {
                    if kind == GraphSymbolKind::Relation && name == "S" {
                        Some(GraphSymbol::Relation(other))
                    } else {
                        symbols(kind, name)
                    }
                },
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            assert_eq!(second.independent_edge_append_relation(), Some(other));
            let query = PreparedGraphWriteProgram::prepare(vec![first, second.into()]).unwrap();
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&c.txn()).unwrap();
            let actual = txn
                .execute_graph_write_program_governed(
                    &mut db,
                    &c.query(),
                    &query,
                    policy(),
                    identity,
                )
                .unwrap();
            assert_eq!(actual.created_edges, 2);
            txn.commit(&mut db, &c.commit()).await.unwrap();
            let relations: Vec<_> = db
                .delta_since(basis)
                .unwrap()
                .flat_map(|batch| batch.coordinate_entries())
                .map(|coordinate| coordinate.relation)
                .collect();
            assert_eq!(relations, vec![R, other]);
            for (eid, relation) in [(EId(100_000), R), (EId(110_000), other)] {
                let edge = db.edge(eid).unwrap().unwrap();
                assert_eq!(edge.entry.relation, relation);
                assert_eq!((edge.entry.src, edge.entry.dst), (VId(1), VId(1)));
                assert_eq!(edge.props, vec![(P, CanonicalScalar::Int(10).into())]);
            }
            assert_eq!(c.txn().outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn late_chunk_refusals_match_the_incumbent_before_any_later_allocation() {
        let ((), report) = run_async_under_lab(0x6d17_0103, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed(&mut db, &c.commit()).await;
            let mut retired = WriteBatch::new(R);
            retired.add_edge(EId(13), VId(1), VId(2), vec![]);
            db.write(&c.commit(), retired).await.unwrap();
            let mut retired = WriteBatch::new(R);
            retired.delete_edge(EId(13));
            db.write(&c.commit(), retired).await.unwrap();
            let value = PreparedGraphInsertText::prepare(
                "MATCH (n:Base) WHERE n.p=10 CREATE (n)-[:R {p:$value}]->(n)",
                R,
                symbols,
            )
            .unwrap();
            for mode in 0..6 {
                let mut statements = loops(3).statements().to_vec();
                if mode == 3 {
                    statements[1] = value
                        .bind_parameters(
                            &GqlParameters::new()
                                .with_scalar(
                                    "value",
                                    CanonicalScalar::bytes(vec![0x5a; 20_000]).unwrap(),
                                )
                                .unwrap(),
                        )
                        .unwrap()
                        .into();
                } else if mode == 4 {
                    statements[1] =
                        program(&["MATCH (n:Base) WHERE n.p=10 CREATE (n)-[:R {p:1/0}]->(n)"])
                            .statements()[0]
                            .clone();
                }
                let query = PreparedGraphWriteProgram::prepare(statements).unwrap();
                let allocate = |request: GraphWriteIdentityRequest| {
                    if request.statement != 1 {
                        return edge_identity(request);
                    }
                    match mode {
                        0 => Ok(ElementId::Edge(EId(u128::MAX))),
                        1 => Ok(ElementId::Edge(EId(11))),
                        2 => Ok(ElementId::Edge(EId(13))),
                        5 => Err("second allocator refused"),
                        _ => edge_identity(request),
                    }
                };
                let mut expected_requests = Vec::new();
                let mut old = db.begin(&c.txn()).unwrap();
                let expected =
                    incumbent(&mut old, &mut db, &c.query(), &query, policy(), |request| {
                        expected_requests.push(request);
                        allocate(request)
                    })
                    .unwrap_err();
                let mut requests = Vec::new();
                let mut txn = db.begin(&c.txn()).unwrap();
                let before = txn.staged_effect_digest().unwrap();
                let actual = txn
                    .execute_graph_write_program_returning_governed(
                        &mut db,
                        &c.query(),
                        &query,
                        policy(),
                        |request| {
                            requests.push(request);
                            allocate(request)
                        },
                    )
                    .unwrap_err();
                assert_eq!(actual.to_string(), expected.to_string(), "mode={mode}");
                assert!(matches!(
                    &actual,
                    GraphWriteProgramError::Insert { statement: 1, .. }
                ));
                assert_eq!(requests, expected_requests, "mode={mode}");
                assert!(requests.iter().all(|request| request.statement < 2));
                assert_eq!(requests.len(), if mode == 4 { 1 } else { 2 });
                assert_eq!(txn.staged_effect_digest().unwrap(), before);
                assert_eq!(txn.state(), EmbeddedTxnState::Active);
                let retry = txn
                    .execute_graph_write_program_governed(
                        &mut db,
                        &c.query(),
                        &loops(3),
                        policy(),
                        edge_identity,
                    )
                    .unwrap();
                assert_eq!(retry.created_edges, 3);
                old.abort();
                txn.abort();
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn exact_program_allowances_and_every_checkpoint_preserve_atomic_retry() {
        let ((), report) = run_async_under_lab(0x6d17_0104, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed(&mut db, &c.commit()).await;
            let query = loops(3);
            let mut warm = db.begin(&c.txn()).unwrap();
            incumbent(
                &mut warm,
                &mut db,
                &c.query(),
                &query,
                policy(),
                edge_identity,
            )
            .unwrap();
            warm.abort();
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let mut measured = db.begin(&c.txn()).unwrap();
            let full = measured
                .execute_graph_write_program_returning_governed(
                    &mut db,
                    &c.query().with_checkpoint_probe(Arc::clone(&probe)),
                    &query,
                    policy(),
                    edge_identity,
                )
                .unwrap();
            let checkpoints = probe.calls();
            let stats = full.stats();
            let exact = GraphWriteProgramPolicy::new(
                GqlQueryPolicy::new(
                    stats.selection.snapshot_records,
                    stats.selection.result_rows,
                    stats.evaluator.work_units,
                    stats.evaluator.scratch_entries,
                ),
                0,
                0,
                stats.created_edges,
            );
            measured.abort();
            for dimension in 0..=5 {
                let mut allowance = exact;
                match dimension {
                    0 | 1 => {
                        allowance.mutations.query.rows = fgdb_gql::GqlExecutionBudget::new(
                            stats.selection.snapshot_records - u64::from(dimension == 0),
                            stats.selection.result_rows - u64::from(dimension == 1),
                        );
                    }
                    2 => allowance.mutations.query.evaluator.max_work_units -= 1,
                    3 => allowance.mutations.query.evaluator.max_scratch_entries -= 1,
                    4 => allowance.max_created_edges -= 1,
                    _ => {}
                }
                let mut txn = db.begin(&c.txn()).unwrap();
                let before = txn.staged_effect_digest().unwrap();
                let result = txn.execute_graph_write_program_returning_governed(
                    &mut db,
                    &c.query(),
                    &query,
                    allowance,
                    edge_identity,
                );
                if dimension == 5 {
                    assert_eq!(result.unwrap().stats(), stats);
                } else {
                    let error = result.unwrap_err();
                    if dimension == 4 {
                        assert!(matches!(
                            error,
                            GraphWriteProgramError::CreationBudget { .. }
                        ));
                    } else {
                        let GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
                            dimension: actual,
                            ..
                        }) = error
                        else {
                            panic!("expected cumulative dimension {dimension}: {error:?}");
                        };
                        assert_eq!(
                            actual,
                            [
                                GraphMutationProgramDimension::SnapshotRecords,
                                GraphMutationProgramDimension::SelectedRows,
                                GraphMutationProgramDimension::WorkUnits,
                                GraphMutationProgramDimension::ScratchEntries,
                            ][dimension]
                        );
                    }
                    assert_eq!(txn.staged_effect_digest().unwrap(), before);
                }
                txn.abort();
            }
            assert!(checkpoints > 3);
            for stop in 1..=checkpoints {
                let mut txn = db.begin(&c.txn()).unwrap();
                txn.savepoint(&db, "before").unwrap();
                let before = txn.staged_effect_digest().unwrap();
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let result = txn.execute_graph_write_program_returning_governed(
                    &mut db,
                    &c.query().with_checkpoint_probe(Arc::clone(&probe)),
                    &query,
                    exact,
                    edge_identity,
                );
                assert!(
                    matches!(
                        result,
                        Err(GraphWriteProgramError::Program(
                            GraphMutationProgramError::Interrupted { .. }
                        )) | Err(GraphWriteProgramError::Insert {
                            source: GqlQueryError::Interrupted(_),
                            ..
                        })
                    ),
                    "checkpoint {stop}/{checkpoints}: {result:?}"
                );
                assert_eq!(probe.calls(), stop);
                assert_eq!(txn.staged_effect_digest().unwrap(), before);
                let retry = txn
                    .execute_graph_write_program_returning_governed(
                        &mut db,
                        &c.query(),
                        &query,
                        exact,
                        edge_identity,
                    )
                    .unwrap();
                assert_eq!(retry.stats(), stats);
                assert_eq!(retry.steps(), full.steps());
                txn.rollback_to_savepoint(&db, "before").unwrap();
                assert_eq!(txn.staged_effect_digest().unwrap(), before);
                txn.abort();
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    /// Paired complete bound-program preparation and normal MemVfs commit.
    /// Binding, fixture creation, cache warmup and oracle inspection are outside
    /// both timers. Every trial starts at the same seeded basis, uses the same
    /// caller allocator/policy and publishes through the ordinary commit path.
    /// This reports observations; wall-clock ratios are never correctness gates.
    #[test]
    #[ignore = "paired 250/1000/4000-record preparation and commit measurement"]
    fn independent_edge_program_paired_measurement() {
        use fgdb::EdgeRecord;
        use fgdb_gql::PreparedGraphWriteScript;
        use std::time::{Duration, Instant};

        struct Measurement {
            preparation: Duration,
            commit: Duration,
            receipt: GraphWriteProgramReceipt,
            digest: fgdb_crypto::Digest,
            edges: Vec<EdgeRecord>,
        }

        async fn measure(
            c: &PurposeContexts,
            query: &PreparedGraphWriteProgram,
            allowance: GraphWriteProgramPolicy,
            independent: bool,
        ) -> Measurement {
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed(&mut db, &c.commit()).await;
            let basis = db.frontier().unwrap();
            let mut warm = db.begin(&c.txn()).unwrap();
            incumbent(
                &mut warm,
                &mut db,
                &c.query(),
                &loops(2),
                policy(),
                edge_identity,
            )
            .unwrap();
            warm.abort();
            let mut txn = db.begin(&c.txn()).unwrap();
            let started = Instant::now();
            let receipt = if independent {
                txn.execute_graph_write_program_returning_governed(
                    &mut db,
                    &c.query(),
                    query,
                    allowance,
                    edge_identity,
                )
            } else {
                incumbent(
                    &mut txn,
                    &mut db,
                    &c.query(),
                    query,
                    allowance,
                    edge_identity,
                )
            }
            .unwrap();
            let preparation = started.elapsed();
            // Inspect the exact prepared authority without charging this
            // benchmark-only encoding to either implementation's elapsed time.
            let digest = txn.staged_effect_digest().unwrap();
            let started = Instant::now();
            txn.commit(&mut db, &c.commit()).await.unwrap();
            let commit = started.elapsed();
            assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
            let edges = db.edges().unwrap();
            assert_eq!(edges.len(), query.statements().len() + 2);
            assert_eq!(c.txn().outstanding_obligations(), 0);
            Measurement {
                preparation,
                commit,
                receipt,
                digest,
                edges,
            }
        }

        fn equivalent(old: &Measurement, new: &Measurement) {
            compare_receipts(&new.receipt, &old.receipt);
            assert_eq!(new.digest, old.digest);
            assert_eq!(new.edges, old.edges);
            assert!(
                new.receipt.stats().evaluator.work_units < old.receipt.stats().evaluator.work_units,
                "the actual ordinary overlay must still walk the staged edge prefix"
            );
        }

        fn summary(mut samples: Vec<Duration>) -> [f64; 3] {
            samples.sort_unstable();
            [
                samples[0],
                samples[samples.len() / 2],
                samples[samples.len() - 1],
            ]
            .map(|time| time.as_secs_f64() * 1000.0)
        }

        let ((), report) = run_async_under_lab(0x6d17_01ff, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let script = PreparedGraphWriteScript::prepare(
                "MATCH (a:Base),(b:Base) WHERE a.p=$source AND b.p=$target CREATE (a)-[:R {p:$value}]->(b)",
                R,
                symbols,
            )
            .unwrap();
            eprintln!(
                "edge_program_measurement storage=MemVfs allocator=deterministic_caller scope=bound_program_preparation_and_commit debug_assertions={} warmup_pairs=1 measured_pairs=5",
                cfg!(debug_assertions)
            );
            for records in [250, 1000, 4000] {
                let args: Vec<_> = (0..records)
                    .map(|record| {
                        let (source, target) = if record % 2 == 0 { (10, 20) } else { (20, 10) };
                        GqlParameters::new()
                            .with_int64("source", source)
                            .unwrap()
                            .with_int64("target", target)
                            .unwrap()
                            .with_int64("value", record as i64)
                            .unwrap()
                    })
                    .collect();
                let batch = script
                    .bind_parameter_sets_with_limit(&args, records)
                    .unwrap();
                let query = batch.program();
                assert_eq!(query.statements().len(), records);
                assert!(query.statements().iter().all(|step| matches!(step,
                    GraphWriteStatement::Insert(input)
                        if input.independent_edge_append_relation() == Some(R)
                )));
                let allowance = GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
                    0,
                    0,
                    records as u64,
                );
                // The expected graph is enumerated independently of either
                // execution: one directed edge per input, plus the seed pair.
                let mut expected = BTreeMap::from([
                    (EId(11), (VId(1), R, VId(2), Vec::new())),
                    (EId(12), (VId(1), R, VId(2), Vec::new())),
                ]);
                for record in 0..records {
                    let eid = if record == 0 {
                        EId(u128::MAX)
                    } else {
                        EId(100_000 + record as u128 * 10_000)
                    };
                    let (source, target) = if record % 2 == 0 {
                        (VId(1), VId(2))
                    } else {
                        (VId(2), VId(1))
                    };
                    expected.insert(
                        eid,
                        (
                            source,
                            R,
                            target,
                            vec![(
                                P,
                                fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Int(
                                    record as i64,
                                )),
                            )],
                        ),
                    );
                }
                let verify_graph = |measured: &Measurement| {
                    let actual: BTreeMap<_, _> = measured
                        .edges
                        .iter()
                        .map(|edge| {
                            (
                                edge.entry.eid,
                                (
                                    edge.entry.src,
                                    edge.entry.relation,
                                    edge.entry.dst,
                                    edge.props.clone(),
                                ),
                            )
                        })
                        .collect();
                    assert_eq!(actual, expected);
                    assert_eq!(measured.receipt.stats().created_edges, records as u64);
                    assert_eq!(measured.receipt.stats().created_vertices, 0);
                };
                let old = measure(&c, query, allowance, false).await;
                let new = measure(&c, query, allowance, true).await;
                equivalent(&old, &new);
                verify_graph(&new);
                let mut old_samples = Vec::new();
                let mut new_samples = Vec::new();
                for pair in 0..5 {
                    let (old, new) = if pair % 2 == 0 {
                        (
                            measure(&c, query, allowance, false).await,
                            measure(&c, query, allowance, true).await,
                        )
                    } else {
                        let new = measure(&c, query, allowance, true).await;
                        let old = measure(&c, query, allowance, false).await;
                        (old, new)
                    };
                    equivalent(&old, &new);
                    verify_graph(&new);
                    eprintln!(
                        "edge_program_pair records={records} pair={} first={} ordinary_prepare_ms={:.3} independent_prepare_ms={:.3} ordinary_commit_ms={:.3} independent_commit_ms={:.3} ordinary_work={} independent_work={}",
                        pair + 1,
                        if pair % 2 == 0 {
                            "ordinary"
                        } else {
                            "independent"
                        },
                        old.preparation.as_secs_f64() * 1000.0,
                        new.preparation.as_secs_f64() * 1000.0,
                        old.commit.as_secs_f64() * 1000.0,
                        new.commit.as_secs_f64() * 1000.0,
                        old.receipt.stats().evaluator.work_units,
                        new.receipt.stats().evaluator.work_units,
                    );
                    old_samples.push(old);
                    new_samples.push(new);
                }
                eprintln!(
                    "edge_program_summary records={records} order=min_median_max_ms ordinary_prepare={:?} independent_prepare={:?} ordinary_commit={:?} independent_commit={:?} ordinary_prepare_plus_commit={:?} independent_prepare_plus_commit={:?}",
                    summary(
                        old_samples
                            .iter()
                            .map(|sample| sample.preparation)
                            .collect()
                    ),
                    summary(
                        new_samples
                            .iter()
                            .map(|sample| sample.preparation)
                            .collect()
                    ),
                    summary(old_samples.iter().map(|sample| sample.commit).collect()),
                    summary(new_samples.iter().map(|sample| sample.commit).collect()),
                    summary(
                        old_samples
                            .iter()
                            .map(|sample| sample.preparation + sample.commit)
                            .collect()
                    ),
                    summary(
                        new_samples
                            .iter()
                            .map(|sample| sample.preparation + sample.commit)
                            .collect()
                    ),
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn failed_private_chunks_keep_unobserved_edge_identity_dependencies() {
        let ((), report) = run_async_under_lab(0x6d17_0105, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            for mode in 0..3 {
                let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
                seed(&mut db, &c.commit()).await;
                let mut extra = WriteBatch::new(R);
                extra.create_vertex(VId(3), vec![], vec![]);
                extra.create_vertex(VId(4), vec![], vec![]);
                extra.add_edge(EId(55), VId(3), VId(4), vec![]);
                db.write(&c.commit(), extra).await.unwrap();
                let mut txn = db.begin(&c.txn()).unwrap();
                let before = txn.staged_effect_digest().unwrap();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    txn.execute_graph_write_program_returning_governed(
                        &mut db,
                        &c.query(),
                        &loops(3),
                        policy(),
                        |request| {
                            if request.statement == 1 {
                                return match mode {
                                    0 => Err("refuse after one private chunk"),
                                    1 => panic!("unwind after one private chunk"),
                                    _ => Ok(ElementId::Edge(EId(55))),
                                };
                            }
                            edge_identity(request)
                        },
                    )
                }));
                if mode == 1 {
                    assert!(result.is_err());
                } else {
                    assert!(result.unwrap().is_err());
                }
                assert_eq!(txn.staged_effect_digest().unwrap(), before);
                // No transaction graph read may repair the discarded witness.
                // Both winner endpoints are outside the Base selection. Only
                // the successful/refused preparation's edge ID overlaps.
                let mut winner = WriteBatch::new(R);
                if mode == 2 {
                    winner.set_edge_property(EId(55), P, Some(CanonicalScalar::Int(1)));
                } else {
                    winner.add_edge(EId(u128::MAX), VId(3), VId(4), vec![]);
                }
                db.write(&c.commit(), winner).await.unwrap();
                assert!(matches!(
                    txn.finish(&mut db, &c.commit()).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert_eq!(c.txn().outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
