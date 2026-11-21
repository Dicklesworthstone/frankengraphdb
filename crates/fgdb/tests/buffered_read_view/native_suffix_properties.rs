//! Native durable runs retain post-seal topology while faulting only the
//! visible winner's demanded property row through the ordinary buffered store.

use super::*;
use fgdb_types::{CanonicalList, CanonicalPropertyValue};

#[test]
fn property_history_streams_through_a_smaller_read_pool() {
    let ((), report) = run_async_under_lab(0x6275_e019, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let mut initial = WriteBatch::new(RELATION);
        for vid in 1..=2 {
            initial.create_vertex(VId(vid), vec![], vec![]);
        }
        initial.add_edge(
            EId(1000),
            VId(2),
            VId(1),
            vec![(PROPERTY, CanonicalScalar::Int(1000))],
        );
        assert_eq!(db.write(&commit, initial).await.unwrap(), CommitSeq(1));
        let run_limits = SealedLimits {
            max_rows: 2,
            max_incidences: 528,
            max_image_bytes: 4096,
            max_property_bytes: 128,
        };
        db.seal_relation(
            &commit,
            &cx,
            RELATION,
            CommitSeq(0),
            run_limits,
            RootReadLimits::default(),
        )
        .await
        .unwrap();
        const EDGES: usize = 512;
        const PAYLOAD_BYTES: usize = 14 * 1024;
        let payload = |eid: u128| {
            // Distinct values also produce distinct immutable property objects.
            CanonicalScalar::ucs_basic_text(&format!("{eid:04}:{}", "x".repeat(PAYLOAD_BYTES)))
                .unwrap()
        };
        let mut additions = WriteBatch::new(RELATION);
        for eid in 1..=EDGES as u128 {
            additions.add_edge(EId(eid), VId(1), VId(2), vec![(PROPERTY, payload(eid))]);
        }
        assert_eq!(db.write(&commit, additions).await.unwrap(), CommitSeq(2));
        let nested = CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![
                CanonicalScalar::Int(7).into(),
                CanonicalPropertyValue::List(
                    CanonicalList::try_new(vec![
                        CanonicalScalar::ucs_basic_text("after").unwrap().into(),
                        CanonicalScalar::Null.into(),
                    ])
                    .unwrap(),
                ),
            ])
            .unwrap(),
        );
        let mut edits = WriteBatch::new(RELATION);
        edits.set_edge_property_value(EId(1), PROPERTY, Some(nested.clone()));
        edits.set_edge_property(EId(2), PROPERTY, None);
        edits.delete_edge(EId(3));
        assert_eq!(db.write(&commit, edits).await.unwrap(), CommitSeq(3));
        drop(db);

        // The compressed base and complete topology directory remain resident.
        // The suffix's property history alone exceeds this entire read pool;
        // retaining every decoded suffix property row cannot pass this test.
        let memory = MemoryPool::new(6 * 1024 * 1024, 0).unwrap();
        assert!(EDGES * PAYLOAD_BYTES > memory.limit());
        let mut source_limits = limits();
        source_limits.max_root_bytes = 192 * 1024;
        source_limits.max_source_bytes = 32 * 1024 * 1024;
        source_limits.max_blocks = 1024;
        source_limits.max_work = 100_000_000;
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            memory.clone(),
            source_limits,
        )
        .await
        .unwrap();
        view.set_native_run_limits(&cx, Some(run_limits)).unwrap();
        let baseline = memory.used();
        let plan = query("MATCH (a)-[r:R]->(b) RETURN r,r.p AS p");
        let allowance = GqlQueryPolicy::new(1024, 1024, 100_000_000, 100_000_000);
        for cut in [1, 2, 3] {
            let mut cursor = view
                .stream_graph_edges_governed_at(&cx, &plan, CommitSeq(cut), allowance)
                .unwrap();
            for eid in (1..=EDGES as u128)
                .filter(|eid| cut >= 2 && !(cut == 3 && *eid == 3))
                .chain(std::iter::once(1000))
            {
                let row = cursor.next().await.unwrap().unwrap();
                let property = match (eid, cut) {
                    (1000, _) => GraphValue::Scalar(CanonicalScalar::Int(1000)),
                    (1, 3) => GraphValue::from_property_value(&nested).unwrap(),
                    (2, 3) => GraphValue::Scalar(CanonicalScalar::Null),
                    _ => GraphValue::Scalar(payload(eid)),
                };
                assert_eq!(
                    row.values(),
                    [GraphValue::Edge(EId(eid)), property],
                    "post-seal property winner {eid} at {cut}"
                );
                assert!(memory.used() <= memory.limit());
            }
            assert!(cursor.next().await.is_none());
            assert_eq!(cursor.state(), EdgeScanState::Exhausted);
            assert_eq!(cursor.row_stats().snapshot_records, EDGES as u64 + 1);
            drop(cursor);
            assert_eq!(memory.used(), baseline);
        }
        // A delivered nested value owns its charge independently of the run,
        // suffix locator and buffered view that produced it.
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &plan, allowance)
            .unwrap();
        let held = cursor.next().await.unwrap().unwrap();
        cursor.close();
        drop(cursor);
        drop(view);
        assert!(memory.used() > 0);
        assert_eq!(
            held.values()[1],
            GraphValue::from_property_value(&nested).unwrap()
        );
        drop(held);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn property_faults_follow_visible_demand_and_fail_terminally() {
    let ((), report) = run_async_under_lab(0x6275_e01a, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, db, _) = native_suffix_history(&commit, &cx).await;
        let store = BlockStore::open_existing_for_read_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
        )
        .await
        .unwrap();
        let root = db.partition_root().unwrap();
        let mut bodies = std::collections::BTreeMap::new();
        for reference in store.get_root(&commit, root).await.unwrap().blocks {
            if reference.last_seq <= CommitSeq(3) {
                continue;
            }
            let bytes = store
                .get_bytes(&commit, fgdb_strata::DeltaBlockVersion(reference.block_id))
                .await
                .unwrap();
            let (entries, properties) = fgdb_strata::decode_block_with_properties(&bytes).unwrap();
            if entries.iter().all(|entry| entry.relation == RELATION)
                && let Some((id, _)) = properties
            {
                let path = store.path(id);
                bodies.insert(path.clone(), vfs.read(&path).await.unwrap());
            }
        }
        assert!(!bodies.is_empty());
        let (mut view, memory) = native_run_view(&commit, &cx, &vfs).await;
        let baseline = memory.used();
        let topology_memory = pool();
        let mut topology_limits = limits();
        topology_limits.max_work = 1_000_000;
        let mut partition = store
            .open_buffered_root(&cx, root, topology_memory.clone(), topology_limits)
            .await
            .unwrap();
        partition
            .set_native_run_limits(&cx, Some(native_run_limits()))
            .unwrap();
        for path in bodies.keys() {
            vfs.write(path, b"damaged after complete initial property admission")
                .await
                .unwrap();
        }
        // New opens still require complete source validation, even if all
        // subsequent demand would have been topology-only.
        assert!(
            store
                .open_buffered_root(&cx, root, pool(), limits())
                .await
                .is_err()
        );
        for cut in 0..=6 {
            let mut scan = partition
                .edge_scan_relation(&cx, CommitSeq(cut), RELATION)
                .unwrap();
            let mut visible = Vec::new();
            while let Some(candidate) = scan
                .next_topology_candidate_with(&cx, &mut |_| Ok::<_, std::convert::Infallible>(()))
                .await
                .unwrap()
            {
                if candidate.row.is_some() {
                    visible.push(candidate.eid);
                }
            }
            let expected: Vec<_> = native_suffix_expected(cut, 0)
                .into_iter()
                .map(|row| match row[0] {
                    GraphValue::Edge(eid) => eid,
                    _ => unreachable!("fixture edge identity"),
                })
                .collect();
            assert_eq!(visible, expected, "topology-only cut {cut}");
        }
        drop(partition);
        assert_eq!(topology_memory.used(), 0);

        let plan = native_run_query(0, ":R", "");
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &plan, native_run_policy(7))
            .unwrap();
        // EId 0's property was removed; EId 42 is retired. Neither demands
        // its older property body, so the first visible winner can be yielded.
        assert_eq!(
            cursor.next().await.unwrap().unwrap().values(),
            native_suffix_expected(6, 0)[0].as_slice()
        );
        let error = cursor.next().await.unwrap().unwrap_err();
        assert!(
            matches!(error, GqlQueryError::Source(EdgeScanError::Source(_))),
            "{error:?}"
        );
        assert_eq!(cursor.row_stats().snapshot_records, 3);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(memory.used(), baseline);

        // The same EId at its older cut really needs that damaged body and
        // cannot substitute the newer missing value or fall back to Tier D.
        let mut cursor = view
            .stream_graph_edges_governed_at(&cx, &plan, CommitSeq(4), native_run_policy(7))
            .unwrap();
        assert!(cursor.next().await.unwrap().is_err());
        assert_eq!(cursor.row_stats().snapshot_records, 1);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(memory.used(), baseline);
        for (path, bytes) in &bodies {
            vfs.write(path, bytes).await.unwrap();
        }
        assert_eq!(
            native_run_rows(&mut view, &cx, &plan, 4, 7).await,
            native_suffix_expected(4, 0)
        );

        // Key-only pulls share the same source-bound winning locator,
        // including a removed key, an absent different key, and base rows.
        let mut partition = store
            .open_buffered_root(&cx, root, topology_memory.clone(), topology_limits)
            .await
            .unwrap();
        partition
            .set_native_run_limits(&cx, Some(native_run_limits()))
            .unwrap();
        for cut in [4, 6] {
            for key in [PROPERTY, PropertyKeyId(2)] {
                let expected = native_suffix_expected(cut, 0);
                let mut scan = partition
                    .edge_scan_relation(&cx, CommitSeq(cut), RELATION)
                    .unwrap();
                for eid in [0, 42, 101, 102, 103, 104, u128::MAX] {
                    let candidate = scan
                        .next_property_candidate_with(&cx, key, &mut |_| {
                            Ok::<_, std::convert::Infallible>(())
                        })
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(candidate.eid, EId(eid));
                    let expected = expected
                        .iter()
                        .find(|row| row[0] == GraphValue::Edge(EId(eid)));
                    match (candidate.row, expected) {
                        (None, None) => {}
                        (Some(actual), Some(expected)) => {
                            let expected = match &expected[3] {
                                GraphValue::Scalar(CanonicalScalar::Null) => None,
                                GraphValue::Scalar(value) if key == PROPERTY => {
                                    Some(CanonicalPropertyValue::Scalar(value.clone()))
                                }
                                _ => None,
                            };
                            assert_eq!(actual.value(), expected.as_ref());
                        }
                        _ => panic!("property visibility mismatch for {eid} at {cut}"),
                    }
                }
                assert!(
                    scan.next_property_candidate_with(&cx, key, &mut |_| {
                        Ok::<_, std::convert::Infallible>(())
                    })
                    .await
                    .unwrap()
                    .is_none()
                );
            }
        }
        drop(partition);
        assert_eq!(topology_memory.used(), 0);

        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
        let mut cursor = view
            .stream_graph_edges_governed_at(&observed, &plan, CommitSeq(4), native_run_policy(7))
            .unwrap();
        drop(cursor.next().await.unwrap().unwrap());
        let checkpoints = probe.calls();
        assert!(checkpoints > 16);
        cursor.close();
        drop(cursor);
        for stop in [checkpoints - 16, checkpoints - 4, checkpoints] {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
            let mut cursor = view
                .stream_graph_edges_governed_at(
                    &interrupted,
                    &plan,
                    CommitSeq(4),
                    native_run_policy(7),
                )
                .unwrap();
            assert!(
                cursor.next().await.unwrap().is_err(),
                "suffix checkpoint {stop}"
            );
            assert_eq!(probe.calls(), stop);
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(cursor.next().await.is_none());
            drop(cursor);
            assert_eq!(memory.used(), baseline, "suffix checkpoint {stop}");
        }
        assert_eq!(
            native_run_rows(&mut view, &cx, &plan, 6, 7).await,
            native_suffix_expected(6, 0)
        );
        drop(view);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
