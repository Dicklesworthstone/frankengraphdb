//! Candidate positions and every actual root/probe/delivery refusal boundary.
use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::sync::Arc;

#[test]
fn equality_posting_enumeration_visits_each_node_once_instead_of_reseeking() {
    for count in [0, 1, 64, 4096, 16384_usize] {
        let expected: Vec<_> = (0..count)
            .map(|id| VId(if id + 1 == count { u128::MAX } else { id as u128 }))
            .collect();
        let index = IndexMap::from_sorted(
            expected.iter().map(|&vid| (vid, ())).collect(),
            &mut 0,
        );
        let mut incumbent = Vec::new();
        let mut old_work = 0;
        let mut after = None;
        while let Some(vid) = index_successor(&index, after, &mut |event| {
            assert!(matches!(event, VertexScanEvent::Work));
            old_work += 1;
            Ok::<_, ()>(())
        })
        .unwrap()
        {
            incumbent.push(vid);
            after = Some(vid);
        }
        assert_eq!(incumbent, expected);

        let mut scan = VertexPostingScanCursor::default();
        let (mut work, mut scratch) = (0, 0);
        let mut actual = Vec::new();
        after = None;
        while let Some(vid) = scan
            .successor(&index, after, &mut |event| {
                match event {
                    SourceEvent::Work => work += 1,
                    SourceEvent::ScratchEntry => scratch += 1,
                    SourceEvent::SnapshotRecord => panic!("enumeration does not admit a row"),
                }
                Ok::<_, ()>(())
            })
            .unwrap()
        {
            actual.push(vid);
            // Another posting may refuse after this one advanced. Retained
            // lookahead must survive a retry with the unchanged global cut.
            assert_eq!(
                scan.successor(&index, after, &mut |_| Err(())),
                Ok(Some(vid))
            );
            after = Some(vid);
        }
        assert_eq!(actual, expected);
        assert_eq!(work, 2 * count + 1, "one descent and advance per identity");
        assert_eq!(
            scratch,
            usize::from(index.height()),
            "the complete spine is charged once"
        );
        if count >= 64 {
            assert!(old_work > 2 * work, "incumbent={old_work}, candidate={work}");
        }
    }
}

#[test]
fn merged_equality_postings_retry_every_refusal_without_losing_a_numeric_peer() {
    let sets = [
        vec![VId(0), VId(17), VId(1_u128 << 100), VId(u128::MAX)],
        vec![VId(0), VId(3), VId(17), VId(21), VId(u128::MAX)],
    ];
    let expected: Vec<_> = sets
        .iter()
        .flatten()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let roots = sets.map(|ids| {
        IndexMap::from_sorted(ids.into_iter().map(|vid| (vid, ())).collect(), &mut 0)
    });
    fn drain(
        roots: &[IndexMap<VId, ()>; 2],
        scans: &mut [VertexPostingScanCursor; 2],
        after: &mut Option<VId>,
        output: &mut Vec<VId>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), usize>,
    ) -> Result<(), usize> {
        loop {
            let mut next = None;
            for (root, scan) in roots.iter().zip(scans.iter_mut()) {
                if let Some(vid) = scan.successor(root, *after, control)? {
                    next = Some(next.map_or(vid, |current: VId| current.min(vid)));
                }
            }
            let Some(vid) = next else { return Ok(()) };
            assert!(after.is_none_or(|previous| previous < vid));
            output.push(vid);
            *after = Some(vid);
        }
    }
    let mut total = 0;
    let mut output = Vec::new();
    drain(
        &roots,
        &mut core::array::from_fn(|_| VertexPostingScanCursor::default()),
        &mut None,
        &mut output,
        &mut |_| {
            total += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(output, expected);
    for stop in 1..=total {
        let mut scans = core::array::from_fn(|_| VertexPostingScanCursor::default());
        let mut after = None;
        let mut output = Vec::new();
        let mut seen = 0;
        let refused = drain(&roots, &mut scans, &mut after, &mut output, &mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert_eq!(refused, Err(stop));
        assert_eq!(seen, stop);
        assert!(expected.starts_with(&output));
        drain(&roots, &mut scans, &mut after, &mut output, &mut |_| Ok(())).unwrap();
        assert_eq!(output, expected, "refused event {stop}");
    }
}

#[test]
fn equality_postings_preserve_numeric_peers_tombstones_and_historical_cuts() {
    let ((), report) = run_async_under_lab(0x696e_6406, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let keys = crate::DatabaseKeys::new(
            [0x81; 32],
            DatabaseSecurityNamespaceId([0x82; 32]),
            [0x83; 32],
        );
        let key = PropertyKeyId(1);
        let int = CanonicalScalar::Int(7);
        let float = CanonicalScalar::Float(fgdb_types::CanonicalF64::new(7.0));
        let wide_id = VId(1_u128 << 100);
        let last_id = VId(u128::MAX);
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut seed = crate::WriteBatch::new(RelationId(1));
        for (vid, value) in [
            (VId(0), int.clone()),
            (VId(17), float.clone()),
            (wide_id, int.clone()),
            (last_id, CanonicalScalar::Int(9)),
        ] {
            seed.create_vertex(vid, vec![], vec![(key, value)]);
        }
        let first = db.write(&commit, seed).await.unwrap();
        let mut update = crate::WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(0), key, Some(CanonicalScalar::Int(9)));
        update.set_vertex_property(VId(17), key, Some(int));
        update.set_vertex_property(last_id, key, Some(float));
        update.delete_vertex(wide_id);
        let second = db.write(&commit, update).await.unwrap();
        let mut update = crate::WriteBatch::new(RelationId(1));
        update.set_vertex_property(last_id, key, Some(CanonicalScalar::Int(9)));
        let third = db.write(&commit, update).await.unwrap();
        let view = db.read_session().unwrap();
        let mut builder = fgdb_gql::algebra::GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        builder
            .filter(
                "n",
                VertexPredicate::IntegerProperty {
                    key,
                    comparison: IntegerComparison::Equal,
                    value: 7,
                },
            )
            .unwrap();
        let pattern = builder.prepare("n", 0, None).unwrap();
        let policy = GqlQueryPolicy::new(100, 100, 100_000, 100_000);
        for (cut, expected) in [
            (CommitSeq(0), vec![]),
            (first, vec![VId(0), VId(17), wide_id]),
            (second, vec![VId(17), last_id]),
            (third, vec![VId(17)]),
        ] {
            let actual = view
                .stream_graph_vertices_governed_at(&cx, &pattern, cut, policy)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(actual, expected, "cut={cut:?}");
            assert_eq!(
                db.execute_graph_pattern_governed_at(&cx, &pattern, cut, policy)
                    .unwrap()
                    .value,
                expected,
                "eager cut={cut:?}",
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn local_successors_preserve_root_position_and_every_lookup_refusal_retries() {
    let ((), report) = run_async_under_lab(0x696e_6404, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let keys = crate::DatabaseKeys::new(
            [0x81; 32],
            DatabaseSecurityNamespaceId([0x82; 32]),
            [0x83; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut batch = crate::WriteBatch::new(RelationId(1));
        for id in [0, 17, 1_u128 << 100, u128::MAX] {
            batch.create_vertex(VId(id), vec![], vec![]);
        }
        db.write(&commit, batch).await.unwrap();
        let mut source = SnapshotVertexSource {
            view: db.read_session().unwrap(),
            cx: &cx,
            as_of: db.frontier().unwrap(),
            after: None,
            candidates: RootCandidates::Histories(VertexHistoryScanCursor::default()),
        };
        assert_eq!(
            source.next_vertex(&mut |_| Ok::<_, usize>(())).unwrap(),
            Some(VId(0))
        );
        for after in [
            None,
            Some(VId(0)),
            Some(VId(17)),
            Some(VId(1_u128 << 100)),
            Some(VId(u128::MAX)),
        ] {
            let mut total = 0;
            let expected = source
                .next_probe_vertex(after, &mut |_| {
                    total += 1;
                    Ok::<_, usize>(())
                })
                .unwrap();
            assert!(total > 0);
            assert_eq!(source.after, Some(VId(0)));
            for stop in 1..=total {
                let mut at = 0;
                let refused = source.next_probe_vertex(after, &mut |_| {
                    at += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                });
                assert!(
                    matches!(refused,Err(EdgeExpansionSourceError::Read(VertexScanSourceError::Control(n))) if n==stop)
                );
                assert_eq!(at, stop);
                assert_eq!(source.after, Some(VId(0)));
                assert_eq!(
                    source
                        .next_probe_vertex(after, &mut |_| Ok::<_, usize>(()))
                        .unwrap(),
                    expected
                );
            }
        }
        assert_eq!(
            source.next_vertex(&mut |_| Ok::<_, usize>(())).unwrap(),
            Some(VId(17))
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_history_and_capture_checkpoints_release_the_one_pin_on_refusal() {
    let ((), report) = run_async_under_lab(0x696e_6405, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let keys = crate::DatabaseKeys::new(
            [0x81; 32],
            DatabaseSecurityNamespaceId([0x82; 32]),
            [0x83; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut batch = crate::WriteBatch::new(RelationId(1));
        for (value, id) in [0, 17, 1_u128 << 100, u128::MAX].into_iter().enumerate() {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(value as i64))],
            );
        }
        let basis = db.write(&commit, batch).await.unwrap();
        let mut edit = crate::WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(17));
        let latest = db.write(&commit, edit).await.unwrap();
        let view = db.read_session().unwrap();
        let wide = GqlQueryPolicy::new(1000, 1000, 100_000, 100_000);
        for cut in [CommitSeq(0), basis, latest] {
            for anti in [false, true] {
                let text = format!(
                    "MATCH (a) WHERE {}EXISTS {{ MATCH (x) WHERE x.p > a.p }} RETURN a",
                    if anti { "NOT " } else { "" }
                );
                let q = PreparedGraphText::prepare(&text, |kind, _: &str| match kind {
                    GraphSymbolKind::Property => Some(GraphSymbol::Property(PropertyKeyId(1))),
                    _ => None,
                })
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
                let plan = VertexScanPlan::compile(q.plan()).unwrap();
                let build = || SnapshotVertexSource {
                    view: view.clone(),
                    cx: &cx,
                    as_of: cut,
                    after: None,
                    candidates: RootCandidates::Histories(VertexHistoryScanCursor::default()),
                };
                let mut total = 0;
                let mut good = VertexScanCursor::new(build(), plan.clone(), wide, || {
                    total += 1;
                    Ok::<_, usize>(())
                });
                let expected = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                drop(good);
                for stop in 1..=total {
                    let pins = Arc::strong_count(&view.snapshot);
                    let mut calls = 0;
                    let mut c = VertexScanCursor::new(build(), plan.clone(), wide, || {
                        calls += 1;
                        if calls == stop { Err(stop) } else { Ok(()) }
                    });
                    assert_eq!(Arc::strong_count(&view.snapshot), pins + 1);
                    let mut prefix = Vec::new();
                    loop {
                        match c.next() {
                            Some(Ok(row)) => prefix.push(row),
                            Some(Err(GqlQueryError::Interrupted(at))) => {
                                assert_eq!(at, stop);
                                break;
                            }
                            other => panic!("interruption became {other:?}"),
                        }
                    }
                    assert_eq!(prefix, expected[..prefix.len()]);
                    assert_eq!(c.row_stats().result_rows, prefix.len() as u64);
                    assert_eq!(c.state(), VertexScanState::Failed);
                    assert!(c.next().is_none());
                    assert_eq!(Arc::strong_count(&view.snapshot), pins);
                    drop(c);
                    assert_eq!(calls, stop);
                    let retry =
                        VertexScanCursor::new(build(), plan.clone(), wide, || Ok::<_, usize>(()));
                    assert_eq!(retry.collect::<Result<Vec<_>, _>>().unwrap(), expected);
                    assert_eq!(db.frontier().unwrap(), latest);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
