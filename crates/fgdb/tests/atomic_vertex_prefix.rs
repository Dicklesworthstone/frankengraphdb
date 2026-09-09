//! Atomically initialize shared endpoints and several edge relations.
//! This exercises the actual ordinary builder, canonical replay and commit.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch, WriteError, WriteMismatchPolicy, WriteTxnError};
use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlEvidenceAuditError, GqlExecutionBudget, GqlQueryPolicy, PreparedGqlQuery};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const Z: RelationId = RelationId(9);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const HIGH: VId = VId((1_u128 << 96) + 7);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xb1; 32], DatabaseSecurityNamespaceId([0xb2; 32]), [0xb3; 32])
}

fn vertices(ensure: bool) -> WriteBatch {
    let mut batch = WriteBatch::new(Z);
    for (vid, value) in [(VId(1), 7), (VId(2), 8), (HIGH, 9), (VId(4), 10)] {
        let props = vec![(P, CanonicalScalar::Int(value))];
        if ensure { batch.ensure_vertex(vid, vec![L], props); }
        else { batch.create_vertex(vid, vec![L], props); }
    }
    batch
}

fn suffixes(ensure: bool) -> Vec<WriteBatch> {
    let mut r = WriteBatch::new(R);
    let mut s = WriteBatch::new(S);
    if ensure {
        r.ensure_edge_by_triple(EId(10), VId(1), VId(2), vec![]);
        s.ensure_edge_by_triple(EId(20), VId(2), HIGH, vec![]);
    } else {
        r.add_edge(EId(10), VId(1), VId(2), vec![]);
        s.add_edge(EId(20), VId(2), HIGH, vec![]);
    }
    // The ensure is a no-op but still occupies an ordinal in R's suffix.
    r.ensure_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(-1))]);
    vec![s, r]
}

fn graph(ensure: bool) -> Vec<WriteBatch> {
    let mut result = vec![vertices(ensure)];
    result.extend(suffixes(ensure));
    result
}

fn query() -> PreparedGqlQuery {
    PreparedGqlQuery::prepare(
        "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c",
        &RelationBind::new().with_relation("R", R).with_relation("S", S),
    ).unwrap()
}

#[test]
fn shared_creations_publish_once_before_dependent_coordinates_with_unique_births() {
    let ((), report) = run_async_under_lab(0xa704_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let pinned = db.read_session().unwrap();
        let prepared = db.prepare_atomic_writes(graph(false)).unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert!(db.vertices().unwrap().is_empty());
        assert!(db.edges().unwrap().is_empty());
        let seq = db.commit_prepared(&cx, prepared).await.unwrap();
        assert_eq!(seq, CommitSeq(1));
        assert_eq!(db.vertices().unwrap().len(), 4);
        assert_eq!(db.edges().unwrap().len(), 2);
        assert_eq!(db.execute_prepared_query(&query()).unwrap(), vec![HIGH]);
        assert!(pinned.execute_prepared_query(&query()).unwrap().is_empty());
        assert!(db.execute_prepared_query_at(&query(), CommitSeq(0)).unwrap().is_empty());
        let changes: Vec<_> = db.delta_since(CommitSeq(0)).unwrap().collect();
        assert_eq!(changes.len(), 1);
        let coordinates = changes[0].coordinate_entries();
        assert_eq!(coordinates.iter().map(|c| c.relation).collect::<Vec<_>>(), vec![R, S]);
        let mut born = std::collections::BTreeMap::new();
        for coordinate in coordinates {
            for row in &coordinate.rows {
                match row {
                    DeltaRow::CreateVertex { vid, birth_ordinal, .. } => {
                        assert_eq!(coordinate.relation, R, "the earliest coordinate owns endpoints");
                        assert!(born.insert(*birth_ordinal, ElementId::Vertex(*vid)).is_none());
                    }
                    DeltaRow::CreateEdge { eid, birth_ordinal, .. } => {
                        assert!(born.insert(*birth_ordinal, ElementId::Edge(*eid)).is_none());
                    }
                    _ => panic!("unexpected initialization effect: {row:?}"),
                }
            }
        }
        assert_eq!(born.keys().copied().collect::<Vec<_>>(), vec![1, 2, 3, 4, 5, 7]);
        assert_eq!(born[&5], ElementId::Edge(EId(10)));
        assert_eq!(born[&7], ElementId::Edge(EId(20)));
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(7))]);
        assert_eq!(db.vertex(HIGH).unwrap().unwrap().birth_ordinal, 3);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ensure_prefixes_are_idempotent_and_preserve_existing_vertex_contents() {
    let ((), report) = run_async_under_lab(0xa704_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(99)], vec![(P, CanonicalScalar::Int(70))]);
        db.write(&cx, seed).await.unwrap();
        let original = db.vertex(VId(1)).unwrap().unwrap();
        db.write_atomic(&cx, graph(true)).await.unwrap();
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap(), original);
        assert_eq!(db.execute_prepared_query(&query()).unwrap(), vec![HIGH]);
        let expected_vertices = db.vertices().unwrap();
        let expected_edges = db.edges().unwrap();
        let versions = db.element_versions().unwrap().clone();
        db.write_atomic(&cx, graph(true)).await.unwrap();
        assert_eq!(db.vertices().unwrap(), expected_vertices);
        assert_eq!(db.edges().unwrap(), expected_edges);
        assert_eq!(db.element_versions().unwrap(), &versions);
        let mut retire = WriteBatch::new(R);
        retire.delete_vertex(VId(4));
        db.write(&cx, retire).await.unwrap();
        let before_refusal = db.frontier().unwrap();
        assert!(matches!(db.write_atomic(&cx, graph(true)).await,
            Err(WriteTxnError::Write(WriteError::IdentitySpent { elem }))
                if elem == ElementId::Vertex(VId(4))));
        assert_eq!(db.frontier().unwrap(), before_refusal);
        assert_eq!(db.edges().unwrap(), expected_edges);

        let mut fresh = Database::open_memory(&cx, keys()).await.unwrap();
        let mut prefix = WriteBatch::new(Z);
        prefix.ensure_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(7))]);
        prefix.ensure_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(999))]);
        prefix.ensure_vertex(VId(2), vec![L], vec![]);
        prefix.ensure_vertex(HIGH, vec![L], vec![]);
        let mut groups = vec![prefix];
        groups.extend(suffixes(true));
        fresh.write_atomic(&cx, groups).await.unwrap();
        assert_eq!(fresh.vertex(VId(1)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(7))]);
        assert_eq!(fresh.vertex(VId(2)).unwrap().unwrap().birth_ordinal, 3);
        assert_eq!(fresh.execute_prepared_query(&query()).unwrap(), vec![HIGH]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn changing_or_erasing_a_shared_prefix_and_conflicting_suffixes_refuse_atomically() {
    let ((), report) = run_async_under_lab(0xa704_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        for mode in 0..5 {
            let mut groups = graph(false);
            match mode {
                0 => { groups[1].set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(99))); }
                1 => { groups[1].set_vertex_label(VId(4), L, false); }
                // VId(4) is unused by the path. Its erased creation must still
                // be caught by complete-prefix verification, not endpoint tests.
                2 => { groups[1].delete_vertex(VId(4)); }
                3 => {
                    groups[1].compare_and_set_vertex_property(VId(4), P,
                        Some(CanonicalScalar::Int(10)), CanonicalScalar::Int(99),
                        WriteMismatchPolicy::AbortWrite);
                }
                _ => { groups[1].add_edge(EId(10), VId(2), HIGH, vec![]); }
            }
            assert!(matches!(db.write_atomic(&cx, groups).await,
                Err(WriteTxnError::AtomicRelationConflict { .. })), "mode={mode}");
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
        }
        db.write_atomic(&cx, graph(false)).await.unwrap();
        assert_eq!(db.execute_prepared_query(&query()).unwrap(), vec![HIGH]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn later_creations_are_not_hoisted_and_oversized_prefixes_never_publish() {
    let ((), report) = run_async_under_lab(0xa704_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let mut r = WriteBatch::new(R);
        r.create_vertex(VId(1), vec![], vec![]);
        r.create_vertex(VId(2), vec![], vec![]);
        r.add_edge(EId(10), VId(1), VId(2), vec![]);
        r.create_vertex(HIGH, vec![], vec![]); // after the prefix boundary
        let mut s = WriteBatch::new(S);
        s.add_edge(EId(20), VId(2), HIGH, vec![]);
        assert!(matches!(db.prepare_atomic_writes(vec![r, s]),
            Err(WriteTxnError::Write(WriteError::DanglingEndpoint { .. }))));
        let mut oversized = vertices(false);
        oversized.create_vertex(VId(99), vec![],
            vec![(P, CanonicalScalar::bytes(vec![0x51; 20_000]).unwrap())]);
        let mut groups = vec![oversized];
        groups.extend(suffixes(false));
        assert!(matches!(db.write_atomic(&cx, groups).await,
            Err(WriteTxnError::Write(WriteError::VertexStorageAdmission { .. }))));
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert!(db.vertices().unwrap().is_empty());
        assert!(db.edges().unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_prefix_negative_reads_survive_intervening_commits_and_foreign_handles() {
    let ((), report) = run_async_under_lab(0xa704_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for conflicts in [false, true] {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            let mut foreign = Database::open_memory(&cx, keys()).await.unwrap();
            let prepared = db.prepare_atomic_writes(graph(false)).unwrap();
            assert!(matches!(foreign.commit_prepared(&cx, prepared.clone()).await,
                Err(WriteError::ForeignPreparedWrite)));
            assert_eq!(foreign.frontier().unwrap(), CommitSeq(0));
            if conflicts {
                let mut collision = WriteBatch::new(R);
                collision.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(99))]);
                db.write(&cx, collision).await.unwrap();
            }
            for id in 70..73 {
                let mut unrelated = WriteBatch::new(R);
                unrelated.create_vertex(VId(id), vec![], vec![]);
                db.write(&cx, unrelated).await.unwrap();
            }
            let before = db.frontier().unwrap();
            let result = db.commit_prepared(&cx, prepared).await;
            if conflicts {
                assert!(matches!(result, Err(WriteError::FirstCommitterWins { .. })));
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertex(VId(2)).unwrap().is_none());
                assert!(db.edges().unwrap().is_empty());
                assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(99))]);
            } else {
                assert_eq!(result.unwrap(), CommitSeq(before.0 + 1));
                assert_eq!(db.execute_prepared_query(&query()).unwrap(), vec![HIGH]);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transaction_prefix_reuses_existing_staging_and_preserves_it_after_rejected_edits() {
    let ((), report) = run_async_under_lab(0xa704_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let pinned = db.read_session().unwrap();
        let baseline = txn_cx.outstanding_obligations();
        let mut txn = db.begin(&txn_cx).unwrap();
        txn.write(&mut db, vertices(false)).unwrap();
        txn.write_atomic(&mut db, suffixes(false)).unwrap();
        let query = query();
        let before = txn.staged_effect_digest().unwrap();
        let artifact = txn.execute_prepared_query_overlay_artifact(&db, &query).unwrap();
        assert_eq!(artifact.rows(), &[HIGH]);
        assert_eq!(txn.execute_prepared_query_budgeted(&db, &query,
            GqlExecutionBudget::new(2, 1)).unwrap().value, vec![HIGH]);
        assert_eq!(txn.execute_prepared_query_governed(&db, &query_cx, &query,
            GqlQueryPolicy::new(2, 1, 100_000, 100_000)).unwrap().value, vec![HIGH]);
        assert!(db.execute_prepared_query(&query).unwrap().is_empty());
        assert!(matches!(txn.write_atomic(&mut foreign, suffixes(false)),
            Err(WriteTxnError::WrongDatabase)));
        let mut invalid = WriteBatch::new(R);
        invalid.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(99)));
        assert!(matches!(txn.write(&mut db, invalid), Err(WriteTxnError::AtomicRelationConflict { .. })));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        txn.audit_prepared_query_overlay_artifact(&db, &query, &artifact.to_bytes()).unwrap();

        let mut append = WriteBatch::new(RelationId(3));
        append.add_edge(EId(30), HIGH, VId(1), vec![]);
        txn.write(&mut db, append).unwrap();
        assert_eq!(txn.edge(&db, EId(30)).unwrap().unwrap().entry.relation, RelationId(3));
        assert!(matches!(txn.audit_prepared_query_overlay_artifact(&db, &query, &artifact.to_bytes()),
            Err(GqlEvidenceAuditError::StagedEffectMismatch)));
        let born = txn.vertex(&db, HIGH).unwrap().unwrap().birth_ordinal;
        assert_eq!(txn.commit(&mut db, &commit).await.unwrap(), CommitSeq(1));
        assert_eq!(txn_cx.outstanding_obligations(), baseline);
        assert_eq!(db.vertex(HIGH).unwrap().unwrap().birth_ordinal, born);
        assert_eq!(db.edges().unwrap().len(), 3);
        assert_eq!(db.execute_prepared_query(&query).unwrap(), vec![HIGH]);
        assert!(pinned.execute_prepared_query(&query).unwrap().is_empty());
        assert_eq!(db.delta_since(CommitSeq(0)).unwrap().count(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn new_endpoints_and_all_relations_recover_together_across_marker_boundaries() {
    use fgdb::CrashPoint;
    let ((), report) = run_async_under_lab(0xa704_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for (name, point, tear, committed) in [
            ("before", Some(CrashPoint::BeforeCapsule), false, false),
            ("capsule", Some(CrashPoint::AfterCapsuleBeforeD1), false, false),
            ("d1", Some(CrashPoint::AfterD1), false, false),
            ("marker-survived", Some(CrashPoint::AfterMarkerBeforeD2), false, true),
            ("marker-torn", Some(CrashPoint::AfterMarkerBeforeD2), true, false),
            ("marker-synced", Some(CrashPoint::AfterMarkerFileSyncBeforeDirectorySync), false, true),
            ("complete", None, false, true),
        ] {
            let path = std::env::temp_dir().join(format!(
                "fgdb-shared-prefix-recovery-{}-{name}", std::process::id()));
            let mut db = Database::create(&cx, &path, keys()).await.unwrap();
            let pinned = db.read_session().unwrap();
            let prepared = db.prepare_atomic_writes(graph(false)).unwrap();
            let result = db.commit_prepared_with_crash(&cx, prepared, point).await;
            assert_eq!(result.is_ok(), point.is_none(), "point must be reached: {name}");
            if matches!(point, Some(CrashPoint::AfterMarkerBeforeD2
                | CrashPoint::AfterMarkerFileSyncBeforeDirectorySync)) {
                assert!(matches!(result, Err(WriteError::CommitOutcomeUnknown { .. })));
                assert!(db.frontier().is_err());
            }
            assert!(pinned.vertices().unwrap().is_empty());
            assert!(pinned.edges().unwrap().is_empty());
            drop(db);
            if tear {
                fgdb_chronicle::CommitCoordinator::<asupersync::fs::UnixVfs>::tear_log_tail_for_test(&path, 1).unwrap();
            }
            // Surviving unflushed bytes are one crash outcome, not a guarantee
            // of persistence before D2. A torn trailer must recover neither.
            let reopened = Database::open(&cx, &path, keys()).await.unwrap();
            assert_eq!(reopened.frontier().unwrap(), CommitSeq(u64::from(committed)), "{name}");
            assert_eq!(reopened.vertices().unwrap().len(), if committed { 4 } else { 0 }, "{name}");
            assert_eq!(reopened.edges().unwrap().len(), if committed { 2 } else { 0 }, "{name}");
            assert_eq!(reopened.delta_since(CommitSeq(0)).unwrap().count(), usize::from(committed));
            assert_eq!(reopened.execute_prepared_query(&query()).unwrap(),
                if committed { vec![HIGH] } else { vec![] });
            let vertices = reopened.vertices().unwrap();
            let edges = reopened.edges().unwrap();
            let versions = reopened.element_versions().unwrap().clone();
            drop(reopened);
            let rebuilt = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
            assert_eq!(rebuilt.vertices().unwrap(), vertices);
            assert_eq!(rebuilt.edges().unwrap(), edges);
            assert_eq!(rebuilt.element_versions().unwrap(), &versions);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn shared_initialization_versions_and_history_survive_updates_compaction_and_rebuild() {
    let ((), report) = run_async_under_lab(0xa704_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let path = std::env::temp_dir().join(format!(
            "fgdb-shared-prefix-compact-{}", std::process::id()));
        let mut db = Database::create(&cx, &path, keys()).await.unwrap();
        let created = db.write_atomic(&cx, graph(false)).await.unwrap();
        let pinned = db.read_session().unwrap();
        let original_births: Vec<_> = db.vertices().unwrap().into_iter()
            .map(|row| (row.vid, row.birth_ordinal)).collect();
        let mut r = WriteBatch::new(R);
        r.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(77)));
        let mut s = WriteBatch::new(S);
        s.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(88)));
        db.write_atomic(&cx, vec![s, r]).await.unwrap();
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(2));
        let retired = db.write(&cx, cascade).await.unwrap();
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(pinned.execute_prepared_query(&query()).unwrap(), vec![HIGH]);
        assert_eq!(db.execute_prepared_query_at(&query(), created).unwrap(), vec![HIGH]);
        let history: Vec<_> = (0..=retired.0).map(|seq| {
            let at = CommitSeq(seq);
            (db.vertices_at(at).unwrap(), db.edges_at(at).unwrap())
        }).collect();
        let versions = db.element_versions().unwrap().clone();
        db.compact(&cx).await.unwrap();
        assert_eq!(db.element_versions().unwrap(), &versions);
        drop(db);
        for rebuild in [false, true] {
            let reopened = if rebuild {
                Database::open_rebuilding(&cx, &path, keys()).await.unwrap()
            } else { Database::open(&cx, &path, keys()).await.unwrap() };
            assert_eq!(reopened.frontier().unwrap(), retired);
            assert_eq!(reopened.element_versions().unwrap(), &versions);
            for (seq, (vertices, edges)) in history.iter().enumerate() {
                assert_eq!(reopened.vertices_at(CommitSeq(seq as u64)).unwrap(), *vertices);
                assert_eq!(reopened.edges_at(CommitSeq(seq as u64)).unwrap(), *edges);
            }
            assert_eq!(reopened.vertices_at(created).unwrap().into_iter()
                .map(|row| (row.vid, row.birth_ordinal)).collect::<Vec<_>>(), original_births);
            assert_eq!(reopened.execute_prepared_query_at(&query(), created).unwrap(), vec![HIGH]);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
