use super::*;
use fgdb_gql::GlaLimitDimension;

#[test]
fn root_vertex_scan_work_is_linear_and_public_refusals_keep_complete_prefixes() {
    let ((), report) = run_async_under_lab(0x51ca_0110, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let prepared = pattern(0, None, false);
        for count in [24, 192_usize] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let expected: Vec<_> = (0..count - 1)
                .map(|id| VId(2 * id as u128))
                .chain([VId(u128::MAX)])
                .collect();
            let mut initial = WriteBatch::new(R);
            for &vid in &expected {
                initial.create_vertex(vid, vec![], vec![]);
            }
            db.write(&commit, initial).await.unwrap();
            let pinned = db.read_session().unwrap();
            // Neither an unstarted nor an already-driven cursor may switch
            // its root or history hint to this later writer generation.
            let mut change = WriteBatch::new(R);
            change.delete_vertex(VId(0));
            change.create_vertex(VId(1), vec![], vec![]);
            db.write(&commit, change).await.unwrap();
            drop(db);

            let n = count as u64;
            let spine_bound = 2 * u64::from(usize::BITS - count.leading_zeros());
            // A complete single-version identity scan spends 2n+1 source
            // enumeration work, one version descent and row read per root,
            // and the cursor's unchanged admission/projection/delivery work.
            // Scratch admission also spends Work: the public total is 10n+h+2.
            let bound = 10 * n + spine_bound + 2;
            let mut full = pinned
                .stream_graph_vertices_governed(
                    &cx,
                    &prepared,
                    GqlQueryPolicy::new(n, n, bound, spine_bound),
                )
                .unwrap();
            assert_eq!(full.row_stats().snapshot_records, 0);
            assert_eq!(full.evaluator_stats().work_units, 0);
            assert_eq!(full.evaluator_stats().scratch_entries, 0);
            assert_eq!(
                full.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            let rows = full.row_stats();
            let events = full.evaluator_stats();
            assert_eq!(rows.snapshot_records, n);
            assert_eq!(rows.result_rows, n);
            assert_eq!(events.work_units, 10 * n + events.scratch_entries + 2);
            assert!(events.scratch_entries <= spine_bound);
            let exact = GqlQueryPolicy::new(n, n, events.work_units, events.scratch_entries);
            let mut replay = pinned
                .stream_graph_vertices_governed(&cx, &prepared, exact)
                .unwrap();
            assert_eq!(
                replay.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            assert_eq!(replay.evaluator_stats(), events);

            for dimension in [
                GlaLimitDimension::WorkUnits,
                GlaLimitDimension::ScratchEntries,
            ] {
                let total = match dimension {
                    GlaLimitDimension::WorkUnits => events.work_units,
                    GlaLimitDimension::ScratchEntries => events.scratch_entries,
                };
                for limit in 0..total {
                    // Exhaust every work cut on the small graph; the larger
                    // graph supplies the growth control and boundary cuts.
                    if count > 24 && ![0, 1, total / 2, total - 1].contains(&limit) {
                        continue;
                    }
                    let mut allowance = exact;
                    match dimension {
                        GlaLimitDimension::WorkUnits => allowance.evaluator.max_work_units = limit,
                        GlaLimitDimension::ScratchEntries => {
                            allowance.evaluator.max_scratch_entries = limit;
                        }
                    }
                    let mut refused = pinned
                        .stream_graph_vertices_governed(&cx, &prepared, allowance)
                        .unwrap();
                    let mut prefix = Vec::new();
                    loop {
                        match refused.next() {
                            Some(Ok(vid)) => prefix.push(vid),
                            Some(Err(GqlQueryError::Evaluator(error))) => {
                                assert_eq!(error.dimension, dimension);
                                assert_eq!(error.limit, limit);
                                assert_eq!(error.observed, u128::from(limit) + 1);
                                break;
                            }
                            other => panic!("{count} roots, {dimension:?}={limit}: {other:?}"),
                        }
                    }
                    assert_eq!(prefix, expected[..prefix.len()]);
                    assert_eq!(refused.row_stats().result_rows, prefix.len() as u64);
                    assert_eq!(refused.state(), VertexScanState::Failed);
                    assert!(refused.next().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
