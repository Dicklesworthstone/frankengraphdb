//! Engine identities are a keyed permutation of per-kind counters
//! (fgdb-hxgm1 channel 2, owner ruling 2026-10-09). Every engine commit
//! records the counters in its Chronicle marker, so a reopen never reissues an
//! identity the handle issued before that commit. Never-committed
//! reservations are not claimed to be durable leases.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    CrashPoint, Database, DatabaseKeys, IdentityPermutation, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest, PreparedGraphInsert};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergeOutcome, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeOutcome, GraphWriteProgramPolicy, PreparedGraphEdgeMergeText,
    PreparedGraphInsertText, PreparedGraphVertexMergeText, PreparedGraphWriteProgram,
    PreparedGraphWriteProgramTemplate,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use std::path::{Path, PathBuf};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const VERTEX: GraphInsertRequest = GraphInsertRequest::Vertex { row: 0, vertex: 0 };
const EDGE: GraphInsertRequest = GraphInsertRequest::Edge { row: 0, edge: 0 };

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xcd; 32],
        DatabaseSecurityNamespaceId([0x7c; 32]),
        [0x71; 32],
    )
}

/// The vertex identity the engine issues for `counter` under [`keys`].
fn vertex(counter: u64) -> VId {
    VId(u128::from(
        IdentityPermutation::vertices(&keys())
            .permute(counter)
            .unwrap(),
    ))
}

/// The edge identity the engine issues for `counter` under [`keys`].
fn edge(counter: u64) -> EId {
    EId(u128::from(
        IdentityPermutation::edges(&keys())
            .permute(counter)
            .unwrap(),
    ))
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-engine-identity-{}-{name}",
        std::process::id()
    ))
}

fn under_lab<Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static)
where
    Fut: Future<Output = ()> + Send + 'static,
{
    let ((), report) = run_async_under_lab(seed, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root)).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn insertion(text: &str) -> PreparedGraphInsert {
    PreparedGraphInsertText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn insert_policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(query_policy(), 100, 100)
}

fn program_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(), 100, 100, 100)
}

/// Explicit client identities, one pair deleted again. The engine never
/// derives its counters from these: explicit identities matter to it only
/// through the existence check.
async fn deleted_explicit(cx: &CommitCx, path: &Path) -> Database {
    let mut db = Database::create(cx, path, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(7), vec![], vec![]);
    seed.create_vertex(VId(1000), vec![], vec![]);
    seed.add_edge(EId(9000), VId(7), VId(1000), vec![]);
    db.write(cx, seed).await.unwrap();
    let mut deletion = WriteBatch::new(R);
    deletion.delete_edge(EId(9000));
    deletion.delete_vertex(VId(1000));
    db.write(cx, deletion).await.unwrap();
    assert!(db.vertex(VId(1000)).unwrap().is_none());
    assert!(db.edge(EId(9000)).unwrap().is_none());
    db
}

#[test]
fn engine_identities_follow_the_counters_across_fast_open_rebuild_and_compaction() {
    under_lab(0xcd70, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        for rebuilding in [false, true] {
            let path = scratch(if rebuilding {
                "delete-rebuild"
            } else {
                "delete-fast"
            });
            drop(deleted_explicit(&commit, &path).await);
            let mut db = if rebuilding {
                Database::open_rebuilding(&commit, &path, keys())
                    .await
                    .unwrap()
            } else {
                Database::open(&commit, &path, keys()).await.unwrap()
            };
            db.compact(&commit).await.unwrap();
            // Explicit-identity commits leave the counters at zero.
            assert_eq!(
                db.allocate_identity(&query, VERTEX).unwrap(),
                ElementId::Vertex(vertex(1))
            );
            assert_eq!(
                db.allocate_identity(&query, EDGE).unwrap(),
                ElementId::Edge(edge(1))
            );
            let mut created = WriteBatch::new(R);
            created.create_vertex(vertex(1), vec![PERSON], vec![(P, CanonicalScalar::Int(11))]);
            created.add_edge(edge(1), VId(7), vertex(1), vec![]);
            db.write(&commit, created).await.unwrap();
            drop(db);

            let mut db = Database::open(&commit, &path, keys()).await.unwrap();
            assert_eq!(
                db.vertex(vertex(1)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(11))]
            );
            let created = db.edge(edge(1)).unwrap().unwrap();
            assert_eq!((created.entry.src, created.entry.dst), (VId(7), vertex(1)));
            assert!(db.vertex(VId(1000)).unwrap().is_none());
            assert!(db.edge(EId(9000)).unwrap().is_none());
            let (_, vertices, edges, completion) = db
                .execute_graph_insert_returning_autocommit_engine_governed(
                    &txcx,
                    &query,
                    &commit,
                    &insertion("CREATE (a:Person {p:12})-[:R]->(b:Person {p:13})"),
                    insert_policy(),
                )
                .await
                .unwrap();
            assert_eq!(vertices, vec![vertex(2), vertex(3)]);
            assert_eq!(edges, vec![edge(2)]);
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            drop(db);
            let db = Database::open_rebuilding(&commit, &path, keys())
                .await
                .unwrap();
            assert_eq!(
                db.vertex(vertex(3)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(13))]
            );
            let created = db.edge(edge(2)).unwrap().unwrap();
            assert_eq!(
                (created.entry.src, created.entry.dst),
                (vertex(2), vertex(3))
            );
            // Every engine identity fits an i64: openCypher id() and Bolt.
            for id in [vertex(1), vertex(2), vertex(3)] {
                assert!(id.0 <= u128::from(IdentityPermutation::MAX) && id.0 != 0);
            }
        }
    });
}

#[test]
fn reopen_never_reissues_an_identity_issued_before_the_last_commit_even_when_folded_away() {
    under_lab(0xcd79, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        for rebuilding in [false, true] {
            let path = scratch(if rebuilding {
                "fold-rebuild"
            } else {
                "fold-open"
            });
            let mut db = Database::create(&commit, &path, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![], vec![]);
            seed.create_vertex(VId(7), vec![], vec![]);
            seed.add_edge(EId(9), VId(1), VId(7), vec![]);
            db.write(&commit, seed).await.unwrap();
            assert_eq!(
                db.allocate_identity(&query, VERTEX).unwrap(),
                ElementId::Vertex(vertex(1))
            );
            assert_eq!(
                db.allocate_identity(&query, EDGE).unwrap(),
                ElementId::Edge(edge(1))
            );
            // Counter 1 of each kind is folded away inside one commit, so it
            // never reaches a partition row. The commit's marker still
            // records it.
            let mut folded = WriteBatch::new(R);
            folded.create_vertex(vertex(1), vec![], vec![]);
            folded.add_edge(edge(1), VId(1), vertex(1), vec![]);
            folded.delete_edge(edge(1));
            folded.delete_vertex(vertex(1));
            folded.delete_edge(EId(9));
            folded.delete_vertex(VId(7));
            db.write(&commit, folded).await.unwrap();
            // Counter 2 is reserved after that commit and never committed.
            assert_eq!(
                db.allocate_identity(&query, VERTEX).unwrap(),
                ElementId::Vertex(vertex(2))
            );
            assert_eq!(
                db.allocate_identity(&query, EDGE).unwrap(),
                ElementId::Edge(edge(2))
            );
            db.compact(&commit).await.unwrap();
            drop(db);
            let mut db = if rebuilding {
                Database::open_rebuilding(&commit, &path, keys())
                    .await
                    .unwrap()
            } else {
                Database::open(&commit, &path, keys()).await.unwrap()
            };
            // The folded identities stay retired; the never-committed
            // reservation is the next issue.
            assert_eq!(
                db.allocate_identity(&query, VERTEX).unwrap(),
                ElementId::Vertex(vertex(2))
            );
            assert_eq!(
                db.allocate_identity(&query, EDGE).unwrap(),
                ElementId::Edge(edge(2))
            );
            let mut recreated = WriteBatch::new(R);
            recreated.create_vertex(vertex(2), vec![], vec![]);
            recreated.add_edge(edge(2), VId(1), vertex(2), vec![]);
            db.write(&commit, recreated).await.unwrap();
            assert!(db.vertex(VId(7)).unwrap().is_none());
            assert!(db.edge(EId(9)).unwrap().is_none());
            assert!(db.edge(edge(2)).unwrap().is_some());
            drop(db);
            let recovered = Database::open_rebuilding(&commit, &path, keys())
                .await
                .unwrap();
            assert!(recovered.vertex(vertex(2)).unwrap().is_some());
            assert!(recovered.edge(edge(2)).unwrap().is_some());
            assert!(recovered.vertex(vertex(1)).unwrap().is_none());
            assert!(recovered.vertex(VId(7)).unwrap().is_none());
            assert!(recovered.edge(EId(9)).unwrap().is_none());
        }
    });
}

#[test]
fn same_basis_staged_engine_inserts_are_disjoint_and_loser_cannot_publish() {
    under_lab(0xcd71, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let path = scratch("same-basis");
        let mut db = deleted_explicit(&commit, &path).await;
        let basis = db.frontier().unwrap();
        let mut first = db.begin(&txcx).unwrap();
        let mut second = db.begin(&txcx).unwrap();
        // Both insertions depend on the same observed vertex population.
        assert_eq!(
            first
                .vertices(&db)
                .unwrap()
                .into_iter()
                .map(|row| row.vid)
                .collect::<Vec<_>>(),
            vec![VId(7)]
        );
        assert_eq!(
            second
                .vertices(&db)
                .unwrap()
                .into_iter()
                .map(|row| row.vid)
                .collect::<Vec<_>>(),
            vec![VId(7)]
        );
        let create = insertion("CREATE (a:Person {p:1})-[:R]->(b:Person {p:2})");
        let (_, first_vertices, first_edges) = first
            .execute_graph_insert_returning_engine_governed(
                &mut db,
                &query,
                &create,
                insert_policy(),
            )
            .unwrap();
        let (_, second_vertices, second_edges) = second
            .execute_graph_insert_returning_engine_governed(
                &mut db,
                &query,
                &create,
                insert_policy(),
            )
            .unwrap();
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(first_vertices, vec![vertex(1), vertex(2)]);
        assert_eq!(first_edges, vec![edge(1)]);
        assert_eq!(second_vertices, vec![vertex(3), vertex(4)]);
        assert_eq!(second_edges, vec![edge(2)]);
        assert!(first.vertex(&db, first_vertices[0]).unwrap().is_some());
        assert!(second.vertex(&db, second_vertices[0]).unwrap().is_some());
        assert!(db.vertex(first_vertices[0]).unwrap().is_none());
        let committed = first.commit(&mut db, &commit).await.unwrap();
        assert!(matches!(
            second.commit(&mut db, &commit).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
        ));
        assert_eq!(db.frontier().unwrap(), committed);
        for id in &second_vertices {
            assert!(db.vertex(*id).unwrap().is_none());
        }
        assert!(db.edge(second_edges[0]).unwrap().is_none());
        // On the opened handle even the losing reservations remain disjoint.
        assert_eq!(
            db.allocate_identity(&query, VERTEX).unwrap(),
            ElementId::Vertex(vertex(5))
        );
        assert_eq!(
            db.allocate_identity(&query, EDGE).unwrap(),
            ElementId::Edge(edge(3))
        );
        drop(db);

        // The winner's marker recorded the counters as they stood at its
        // commit, the loser's reservations included, so a reopen issues past
        // both transactions.
        let mut db = Database::open(&commit, &path, keys()).await.unwrap();
        let (_, vertices, edges, _) = db
            .execute_graph_insert_returning_autocommit_engine_governed(
                &txcx,
                &query,
                &commit,
                &create,
                insert_policy(),
            )
            .await
            .unwrap();
        assert_eq!(vertices, vec![vertex(5), vertex(6)]);
        assert_eq!(edges, vec![edge(3)]);
        drop(db);
        let db = Database::open_rebuilding(&commit, &path, keys())
            .await
            .unwrap();
        for id in first_vertices.iter().chain(vertices.iter()) {
            assert!(db.vertex(*id).unwrap().is_some());
        }
        for id in first_edges.iter().chain(edges.iter()) {
            assert!(db.edge(*id).unwrap().is_some());
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn d1_d2_recovery_reads_the_counters_of_the_last_durable_marker() {
    under_lab(0xcd72, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        for (name, point, tear, committed) in [
            (
                "before-d1",
                Some(CrashPoint::AfterCapsuleBeforeD1),
                false,
                false,
            ),
            ("after-d1", Some(CrashPoint::AfterD1), false, false),
            (
                "marker-survived",
                Some(CrashPoint::AfterMarkerBeforeD2),
                false,
                true,
            ),
            (
                "marker-torn",
                Some(CrashPoint::AfterMarkerBeforeD2),
                true,
                false,
            ),
            (
                "marker-synced",
                Some(CrashPoint::AfterMarkerFileSyncBeforeDirectorySync),
                false,
                true,
            ),
            ("d2-complete", None, false, true),
        ] {
            let path = scratch(name);
            let mut db = deleted_explicit(&commit, &path).await;
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let create = insertion("CREATE (a:Person {p:21})-[:R]->(b:Person {p:22})");
            let (_, vertices, edges) = txn
                .execute_graph_insert_returning_engine_governed(
                    &mut db,
                    &query,
                    &create,
                    insert_policy(),
                )
                .unwrap();
            assert_eq!(vertices, vec![vertex(1), vertex(2)]);
            assert_eq!(edges, vec![edge(1)]);
            let result = txn.commit_with_crash(&mut db, &commit, point).await;
            assert_eq!(result.is_ok(), point.is_none(), "injection reached: {name}");
            if matches!(
                point,
                Some(
                    CrashPoint::AfterMarkerBeforeD2
                        | CrashPoint::AfterMarkerFileSyncBeforeDirectorySync
                )
            ) {
                assert!(matches!(
                    result,
                    Err(WriteTxnError::Write(
                        WriteError::CommitOutcomeUnknown { .. }
                    ))
                ));
                assert!(db.frontier().is_err());
            }
            drop(db);
            if tear {
                // Existing fault helper models a torn marker, not a new power-loss oracle.
                fgdb_chronicle::CommitCoordinator::<asupersync::fs::UnixVfs>::tear_log_tail_for_test(
                    &path, 1,
                ).unwrap();
            }
            // A durable marker carries counters (2, 1). Without one, the last
            // marker is the fixture's, at (0, 0), and the lost commit's
            // identities were never durable.
            let next_vertex = if committed { 3 } else { 1 };
            let next_edge = if committed { 2 } else { 1 };
            for rebuilding in [false, true] {
                let mut recovered = if rebuilding {
                    Database::open_rebuilding(&commit, &path, keys())
                        .await
                        .unwrap()
                } else {
                    Database::open(&commit, &path, keys()).await.unwrap()
                };
                assert_eq!(
                    recovered.frontier().unwrap(),
                    CommitSeq(basis.0 + u64::from(committed)),
                    "{name}"
                );
                for id in &vertices {
                    assert_eq!(
                        recovered.vertex(*id).unwrap().is_some(),
                        committed,
                        "{name}"
                    );
                }
                assert_eq!(
                    recovered.edge(edges[0]).unwrap().is_some(),
                    committed,
                    "{name}"
                );
                assert_eq!(
                    recovered.allocate_identity(&query, VERTEX).unwrap(),
                    ElementId::Vertex(vertex(next_vertex)),
                    "{name}"
                );
                assert_eq!(
                    recovered.allocate_identity(&query, EDGE).unwrap(),
                    ElementId::Edge(edge(next_edge)),
                    "{name}"
                );
            }
            let mut recovered = Database::open(&commit, &path, keys()).await.unwrap();
            let (_, vertices, edges, completion) = recovered
                .execute_graph_insert_returning_autocommit_engine_governed(
                    &txcx,
                    &query,
                    &commit,
                    &create,
                    insert_policy(),
                )
                .await
                .unwrap();
            assert_eq!(
                vertices,
                vec![vertex(next_vertex), vertex(next_vertex + 1)],
                "{name}"
            );
            assert_eq!(edges, vec![edge(next_edge)], "{name}");
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            drop(recovered);
            let recovered = Database::open_rebuilding(&commit, &path, keys())
                .await
                .unwrap();
            let created = recovered.edge(edges[0]).unwrap().unwrap();
            assert_eq!(
                (created.entry.src, created.entry.dst),
                (vertices[0], vertices[1]),
                "{name}"
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
}

fn merge_program() -> PreparedGraphWriteProgram {
    let left =
        PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$left})", R, symbols).unwrap();
    let right =
        PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$right})", R, symbols).unwrap();
    let merge_edge = PreparedGraphEdgeMergeText::prepare(
        "MATCH (a:Person),(b:Person) WHERE a.p=$left AND b.p=$right MERGE (a)-[:R]->(b)",
        R,
        symbols,
    )
    .unwrap();
    PreparedGraphWriteProgramTemplate::prepare(vec![
        left.into(),
        right.into(),
        merge_edge.clone().into(),
        merge_edge.into(),
    ])
    .unwrap()
    .bind_parameters(
        &GqlParameters::new()
            .with_int64("left", 31)
            .unwrap()
            .with_int64("right", 32)
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn engine_insert_create_and_merge_programs_need_no_caller_allocator() {
    under_lab(0xcd73, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let path = scratch("programs");
        let mut db = deleted_explicit(&commit, &path).await;
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn
            .execute_graph_insert_engine_governed(
                &mut db,
                &query,
                &insertion("CREATE (n:Person {p:30})"),
                insert_policy(),
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.vertex(vertex(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(30))]
        );
        let (stats, completion) = db
            .execute_graph_insert_autocommit_engine_governed(
                &txcx,
                &query,
                &commit,
                &insertion("CREATE (n:Person {p:33})"),
                insert_policy(),
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));

        let program = merge_program();
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn
            .execute_graph_write_program_returning_engine_governed(
                &mut db,
                &query,
                &program,
                program_policy(),
            )
            .unwrap();
        assert_eq!(
            receipt.steps()[0].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(vertex(3)))
        );
        assert_eq!(
            receipt.steps()[1].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(vertex(4)))
        );
        assert_eq!(
            receipt.steps()[2].merged_edge(),
            Some(GraphEdgeMergeOutcome::Created(edge(1)))
        );
        assert_eq!(
            receipt.steps()[3].merged_edge(),
            Some(GraphEdgeMergeOutcome::Matched(edge(1)))
        );
        txn.commit(&mut db, &commit).await.unwrap();
        drop(db);
        let mut db = Database::open(&commit, &path, keys()).await.unwrap();
        let merged = db.edge(edge(1)).unwrap().unwrap();
        assert_eq!(
            (merged.entry.src, merged.entry.dst, merged.entry.relation),
            (vertex(3), vertex(4), R)
        );
        let frontier = db.frontier().unwrap();
        let (receipt, completion) = db
            .execute_graph_write_program_returning_autocommit_engine_governed(
                &txcx,
                &query,
                &commit,
                &program,
                program_policy(),
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.steps()[0].merged_vertex(),
            Some(GraphVertexMergeOutcome::Matched(vertex(3)))
        );
        assert_eq!(
            receipt.steps()[1].merged_vertex(),
            Some(GraphVertexMergeOutcome::Matched(vertex(4)))
        );
        for step in &receipt.steps()[2..] {
            assert_eq!(
                step.merged_edge(),
                Some(GraphEdgeMergeOutcome::Matched(edge(1)))
            );
        }
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn
            .execute_graph_write_program_engine_governed(
                &mut db,
                &query,
                &program,
                program_policy(),
            )
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        let (stats, completion) = db
            .execute_graph_write_program_autocommit_engine_governed(
                &txcx,
                &query,
                &commit,
                &program,
                program_policy(),
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(
            db.allocate_identity(&query, VERTEX).unwrap(),
            ElementId::Vertex(vertex(5))
        );
        assert_eq!(
            db.allocate_identity(&query, EDGE).unwrap(),
            ElementId::Edge(edge(2))
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

/// Explicit identities outside the engine's domain `[1, 2^63)` can neither
/// collide with an engine identity nor exhaust the allocator, whether they
/// are live, deleted, or compacted away.
#[test]
fn explicit_identities_outside_the_engine_domain_neither_collide_nor_exhaust() {
    under_lab(0xcd74, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let path = scratch("out-of-domain");
        let mut db = Database::create(&commit, &path, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(u128::MAX), vec![], vec![]);
        seed.add_edge(EId(u128::MAX), VId(1), VId(u128::MAX), vec![]);
        db.write(&commit, seed).await.unwrap();
        assert_eq!(
            db.allocate_identity(&query, VERTEX).unwrap(),
            ElementId::Vertex(vertex(1))
        );
        let mut txn = db.begin(&txcx).unwrap();
        assert_eq!(
            txn.allocate_identity(&mut db, &query, EDGE).unwrap(),
            ElementId::Edge(edge(1))
        );
        let mut issued = WriteBatch::new(R);
        issued.add_edge(edge(1), VId(1), VId(u128::MAX), vec![]);
        txn.write(&mut db, issued).unwrap();
        txn.commit(&mut db, &commit).await.unwrap();
        let mut deletion = WriteBatch::new(R);
        deletion.delete_vertex(VId(u128::MAX));
        db.write(&commit, deletion).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        for rebuilding in [false, true] {
            let mut db = if rebuilding {
                Database::open_rebuilding(&commit, &path, keys())
                    .await
                    .unwrap()
            } else {
                Database::open(&commit, &path, keys()).await.unwrap()
            };
            assert!(db.vertex(VId(u128::MAX)).unwrap().is_none());
            assert!(db.edge(EId(u128::MAX)).unwrap().is_none());
            assert!(db.edge(edge(1)).unwrap().is_none());
            // The deletion commit recorded (1, 1), so issue continues at 2.
            assert_eq!(
                db.allocate_identity(&query, VERTEX).unwrap(),
                ElementId::Vertex(vertex(2))
            );
            assert_eq!(
                db.allocate_identity(&query, EDGE).unwrap(),
                ElementId::Edge(edge(2))
            );
        }
    });
}

/// The existence lookup (owner ruling 2026-10-09): an identity the engine
/// would issue next, already taken by an explicit client creation, is
/// skipped, and its counter is spent.
#[test]
fn an_explicit_identity_at_the_next_engine_identity_is_skipped() {
    under_lab(0xcd75, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let path = scratch("existence");
        let mut db = Database::create(&commit, &path, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(vertex(1), vec![], vec![]);
        seed.add_edge(edge(1), VId(1), vertex(1), vec![]);
        db.write(&commit, seed).await.unwrap();
        assert_eq!(
            db.allocate_identity(&query, VERTEX).unwrap(),
            ElementId::Vertex(vertex(2))
        );
        assert_eq!(
            db.allocate_identity(&query, EDGE).unwrap(),
            ElementId::Edge(edge(2))
        );
        // Deleted but retained, the explicit identities still block reissue.
        let mut deletion = WriteBatch::new(R);
        deletion.delete_edge(edge(1));
        deletion.delete_vertex(vertex(1));
        db.write(&commit, deletion).await.unwrap();
        drop(db);
        let mut db = Database::open(&commit, &path, keys()).await.unwrap();
        // The deletion commit recorded (2, 2).
        assert_eq!(
            db.allocate_identity(&query, VERTEX).unwrap(),
            ElementId::Vertex(vertex(3))
        );
        assert_eq!(
            db.allocate_identity(&query, EDGE).unwrap(),
            ElementId::Edge(edge(3))
        );
    });
}

/// Order hiding, the reason channel 2 exists: a capability that creates two
/// records with k hidden creations between them must not learn k from the
/// two identities. For one key and a fixed counter, the gap between the two
/// identities as a function of k is not affine, and its low byte is not
/// monotone. A deterministic check under the fixture key, not a statistical
/// claim. The identity permutation fails both.
#[test]
fn consecutive_engine_identities_do_not_reveal_how_many_creations_came_between() {
    for permutation in [
        IdentityPermutation::vertices(&keys()),
        IdentityPermutation::edges(&keys()),
    ] {
        let issued = |counter: u64| i128::from(permutation.permute(counter).unwrap());
        let base = issued(100);
        let gaps: Vec<i128> = (1..=64).map(|k| issued(100 + k) - base).collect();
        let step = gaps[0];
        assert!(
            gaps.iter().zip(1i128..).any(|(gap, k)| *gap != k * step),
            "identity gaps are affine in the number of hidden creations"
        );
        let low: Vec<i128> = gaps.iter().map(|gap| gap & 0xff).collect();
        assert!(
            low.windows(2).any(|pair| pair[1] < pair[0]),
            "the low byte of the gap grows with the number of hidden creations"
        );
    }
}

/// The permutation is a bijection on `[1, 2^63)`, depends on the key and the
/// kind, and refuses everything outside its domain.
#[test]
fn the_identity_permutation_is_a_keyed_bijection_on_the_engine_domain() {
    let vertices = IdentityPermutation::vertices(&keys());
    let edges = IdentityPermutation::edges(&keys());
    let other = IdentityPermutation::vertices(&DatabaseKeys::new(
        [0xce; 32],
        DatabaseSecurityNamespaceId([0x7c; 32]),
        [0x71; 32],
    ));
    let mut seen = std::collections::BTreeSet::new();
    for counter in (1..=2_000).chain([IdentityPermutation::MAX - 1, IdentityPermutation::MAX]) {
        let identity = vertices.permute(counter).unwrap();
        assert!((1..=IdentityPermutation::MAX).contains(&identity));
        assert_eq!(vertices.invert(identity), Some(counter));
        assert!(
            seen.insert(identity),
            "counter {counter} reissued an identity"
        );
    }
    let differs = |left: &IdentityPermutation, right: &IdentityPermutation| {
        (1..=64).any(|counter| left.permute(counter) != right.permute(counter))
    };
    assert!(
        differs(&vertices, &edges),
        "vertex and edge keys are separated"
    );
    assert!(
        differs(&vertices, &other),
        "another database key issues another sequence"
    );
    assert_eq!(
        (1..=64)
            .map(|counter| vertices.permute(counter))
            .collect::<Vec<_>>(),
        (1..=64)
            .map(|counter| IdentityPermutation::vertices(&keys()).permute(counter))
            .collect::<Vec<_>>(),
        "the same key replays the same sequence"
    );
    for outside in [0, IdentityPermutation::MAX + 1, u64::MAX] {
        assert_eq!(vertices.permute(outside), None);
        assert_eq!(vertices.invert(outside), None);
    }
    assert_eq!(
        format!("{vertices:?}"),
        "IdentityPermutation([REDACTED])",
        "Debug never prints the key"
    );
}

/// Known answers from an independent implementation: a pure-Python spelling
/// of the construction with its own BLAKE3 compression, keyed hash and
/// derive_key (checked against the BLAKE3 empty-input vector). It also
/// reproduced every engine identity the restated laws observed, under four
/// other key sets. Issued identities are durable, so a silent change to the
/// rounds, half widths, key context or round input must fail here rather
/// than quietly start issuing another sequence.
#[test]
fn the_identity_permutation_matches_independent_known_answers() {
    let vertices = IdentityPermutation::vertices(&keys());
    let edges = IdentityPermutation::edges(&keys());
    for (counter, vertex, edge) in [
        (1, 7_900_765_196_101_795_985, 6_940_111_937_711_507_570),
        (2, 2_172_206_359_520_291_676, 1_270_560_709_950_562_404),
        (3, 1_823_503_639_948_995_673, 414_856_192_363_316_142),
        (1_000, 5_342_259_167_322_058_657, 6_560_166_291_354_285_435),
        (
            IdentityPermutation::MAX,
            6_557_777_098_662_460_084,
            799_399_801_540_463_107,
        ),
    ] {
        assert_eq!(vertices.permute(counter), Some(vertex), "vertex {counter}");
        assert_eq!(edges.permute(counter), Some(edge), "edge {counter}");
    }
}
