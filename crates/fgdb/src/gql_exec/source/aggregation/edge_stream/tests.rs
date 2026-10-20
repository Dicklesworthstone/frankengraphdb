use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

type EdgeImage = (
    EId,
    VId,
    RelationId,
    VId,
    Vec<(PropertyKeyId, CanonicalScalar)>,
);
fn read_source(
    view: EmbeddedReadView,
    cx: &QueryCx,
    as_of: CommitSeq,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), usize>,
) -> Result<Vec<EdgeImage>, EdgeScanSourceError<ReadError, usize>> {
    let mut source = SnapshotEdgeSource {
        view,
        cx,
        as_of,
        scan: IdentityScanCursor::default(),
    };
    let mut rows = Vec::new();
    while let Some(eid) = source.next_edge(control)? {
        let Some(row) = source.edge(eid, control)? else {
            continue;
        };
        for vid in [row.source, row.target] {
            let vertex = source.vertex(vid, control)?.expect("live visible endpoint");
            assert!(vertex.labels.windows(2).all(|pair| pair[0] < pair[1]));
        }
        rows.push((
            eid,
            row.source,
            row.relation,
            row.target,
            row.properties.to_vec(),
        ));
    }
    Ok(rows)
}

#[test]
fn history_predecessors_equal_authoritative_rows_and_every_source_boundary_propagates_refusal() {
    let ((), report) = run_async_under_lab(0xed6e_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let keys = crate::DatabaseKeys::new(
            [0xa7; 32],
            DatabaseSecurityNamespaceId([0xa8; 32]),
            [0xa9; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let relation = RelationId(7);
        let key = PropertyKeyId(9);
        let mut batch = crate::WriteBatch::new(relation);
        for id in [VId(0), VId(u128::MAX)] {
            batch.create_vertex(id, vec![], vec![(key, CanonicalScalar::Int(1))]);
        }
        for eid in [EId(0), EId(1), EId(u128::MAX)] {
            batch.add_edge(
                eid,
                VId(0),
                VId(u128::MAX),
                vec![(key, CanonicalScalar::Int(3))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let old = db.read_session().unwrap();
        // Force a flat base before writes so the next generation exercises
        // overlay ties, not merely a rebuilt flat history for every identity.
        assert_eq!(old.snapshot.adjacency_index().base.ids.len(), 3);
        for round in 0..4 {
            let mut update = crate::WriteBatch::new(relation);
            update.set_edge_property(EId(0), key, Some(CanonicalScalar::Int(round + 4)));
            update.set_vertex_property(VId(0), key, Some(CanonicalScalar::Int(round + 5)));
            if round == 1 {
                update.delete_edge(EId(1));
            }
            db.write(&commit, update).await.unwrap();
        }
        let view = db.read_session().unwrap();
        for cut in 0..=db.frontier().unwrap().0 {
            let as_of = CommitSeq(cut);
            let mut expected: Vec<_> = db
                .edges_at(as_of)
                .unwrap()
                .into_iter()
                .map(|row| {
                    (
                        row.entry.eid,
                        row.entry.src,
                        row.entry.relation,
                        row.entry.dst,
                        row.props,
                    )
                })
                .collect();
            expected.sort_by_key(|row| row.0);
            let mut total = 0;
            let actual = read_source(view.clone(), &cx, as_of, &mut |_| {
                total += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(actual, expected);
            for stop in 1..=total {
                let mut at = 0;
                let refused = read_source(view.clone(), &cx, as_of, &mut |_| {
                    at += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                });
                assert!(
                    matches!(refused, Err(EdgeScanSourceError::Control(value)) if value == stop)
                );
                assert_eq!(at, stop);
                assert_eq!(
                    read_source(view.clone(), &cx, as_of, &mut |_| Ok(())).unwrap(),
                    expected
                );
            }
        }
        let expected_old = old.edges().unwrap();
        let old_cut = old.frontier();
        let old_rows = read_source(old, &cx, old_cut, &mut |_| Ok(())).unwrap();
        assert_eq!(old_rows.len(), expected_old.len());
        assert!(
            old_rows
                .iter()
                .all(|row| row.4 == vec![(key, CanonicalScalar::Int(3))])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sequential_identity_merge_is_linear_and_retries_each_refused_advance() {
    use crate::gql_exec::source::AdjacencyIndex;
    use fgdb_strata::AdjacencyEntry;
    use std::collections::BTreeSet;

    let entry = |eid, created| AdjacencyEntry {
        src: VId(1),
        relation: RelationId(7),
        dst: VId(2),
        eid: EId(eid),
        created_at: CommitSeq(created),
        retired_at: None,
    };
    let mut base: Vec<_> = (0..32).map(|id| entry(id * 2, 1)).collect();
    base.push(entry(u128::MAX, 1));
    let mut overlay: Vec<_> = (0..32).map(|id| entry(id * 2 + 1, 2)).collect();
    overlay.extend((0..32).step_by(3).map(|id| entry(id * 2, 2)));
    overlay.push(entry(u128::MAX, 2));
    let blocks = vec![base, overlay];
    let empty = AdjacencyIndex::build(&blocks[..0]);
    let flat = AdjacencyIndex::build(&blocks[..1]);
    let mixed = flat.extend(&blocks, 1);
    let tree = empty.extend(&blocks, 0);

    for (index, source_blocks) in [
        (&empty, &blocks[..0]),
        (&flat, &blocks[..1]),
        (&mixed, &blocks[..]),
        (&tree, &blocks[..]),
    ] {
        let expected: Vec<_> = source_blocks
            .iter()
            .flatten()
            .map(|entry| entry.eid)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut cursor = IdentityScanCursor::default();
        assert!(!cursor.initialized);
        assert_eq!(
            cursor.overlay.capacity(),
            0,
            "opening visits and allocates nothing"
        );
        let mut event_counts = Vec::new();
        let (mut work, mut scratch) = (0, 0);
        for expected_id in expected.iter().copied().map(Some).chain([None]) {
            let mut events = 0;
            let actual = cursor
                .next(index, &mut |event| {
                    events += 1;
                    match event {
                        SourceEvent::Work => work += 1,
                        SourceEvent::ScratchEntry => scratch += 1,
                        SourceEvent::SnapshotRecord => {
                            panic!("a physical cursor cannot admit rows")
                        }
                    }
                    Ok::<_, usize>(())
                })
                .unwrap();
            assert_eq!(actual, expected_id);
            if let Some(eid) = actual {
                // An overlapping identity must carry the overlay's complete
                // history, including versions from before its extension.
                let expected_history = index.history(eid).unwrap().coordinates();
                assert_eq!(
                    cursor.history(index, eid).unwrap().coordinates(),
                    expected_history
                );
            }
            event_counts.push(events);
        }
        assert!(
            work <= 2 * expected.len() + 1,
            "sequential scan work {work} for {} identities",
            expected.len()
        );
        assert_eq!(scratch, usize::from(index.histories.height()));
        assert!(cursor.overlay.capacity() <= usize::from(index.histories.height()));

        for (position, events) in event_counts.into_iter().enumerate() {
            for stop in 1..=events {
                let mut retry = IdentityScanCursor::default();
                for &eid in &expected[..position] {
                    assert_eq!(
                        retry.next(index, &mut |_| Ok::<_, usize>(())).unwrap(),
                        Some(eid)
                    );
                }
                let previous = position.checked_sub(1).map(|at| expected[at]);
                let mut calls = 0;
                assert_eq!(
                    retry.next(index, &mut |_| {
                        calls += 1;
                        if calls == stop { Err(stop) } else { Ok(()) }
                    }),
                    Err(stop)
                );
                assert_eq!(calls, stop);
                if let Some(eid) = previous {
                    assert!(
                        retry.history(index, eid).is_some(),
                        "refusal replaced the prior history hint"
                    );
                }
                for &eid in &expected[position..] {
                    assert_eq!(
                        retry.next(index, &mut |_| Ok::<_, usize>(())).unwrap(),
                        Some(eid)
                    );
                }
                assert_eq!(
                    retry.next(index, &mut |_| Ok::<_, usize>(())).unwrap(),
                    None
                );
                assert_eq!(
                    retry.next(index, &mut |_| Ok::<_, usize>(())).unwrap(),
                    None
                );
            }
        }
    }
}

#[test]
fn current_history_hint_preserves_arbitrary_reads_and_pinned_generations() {
    let ((), report) = run_async_under_lab(0xed6e_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let keys = crate::DatabaseKeys::new(
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
            [0xb3; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut create = crate::WriteBatch::new(RelationId(7));
        for vid in [VId(1), VId(2)] {
            create.create_vertex(vid, vec![], vec![]);
        }
        let property = PropertyKeyId(9);
        for eid in [EId(0), EId(2), EId(u128::MAX)] {
            create.add_edge(
                eid,
                VId(1),
                VId(2),
                vec![(property, CanonicalScalar::Int(1))],
            );
        }
        let before = db.write(&commit, create).await.unwrap();
        let pinned = db.read_session().unwrap();
        let mut old = pinned.edge_scan_source(&cx, before).unwrap();
        let control = &mut |_| Ok::<_, ()>(());
        assert_eq!(old.next_edge(control).unwrap(), Some(EId(0)));

        let mut change = crate::WriteBatch::new(RelationId(7));
        change.set_edge_property(EId(0), property, Some(CanonicalScalar::Int(2)));
        change.delete_edge(EId(2));
        change.add_edge(
            EId(1),
            VId(2),
            VId(1),
            vec![(property, CanonicalScalar::Int(3))],
        );
        let after = db.write(&commit, change).await.unwrap();
        let mut new = db
            .read_session()
            .unwrap()
            .edge_scan_source(&cx, after)
            .unwrap();
        assert_eq!(new.next_edge(control).unwrap(), Some(EId(0)));
        for (source, value) in [(&old, 1), (&new, 2)] {
            assert_eq!(
                source.edge(EId(0), control).unwrap().unwrap().properties,
                &[(property, CanonicalScalar::Int(value))]
            );
            // Both an arbitrary future candidate and an absent identity take
            // the existing point lookup, without overwriting the root hint.
            assert!(source.edge(EId(u128::MAX), control).unwrap().is_some());
            assert!(source.edge(EId(5), control).unwrap().is_none());
            assert_eq!(
                source.edge(EId(0), control).unwrap().unwrap().properties,
                &[(property, CanonicalScalar::Int(value))]
            );
        }
        db.compact(&commit).await.unwrap();
        drop(db);

        assert_eq!(old.next_edge(control).unwrap(), Some(EId(2)));
        assert!(old.edge(EId(2), control).unwrap().is_some());
        assert_eq!(new.next_edge(control).unwrap(), Some(EId(1)));
        assert_eq!(new.edge(EId(1), control).unwrap().unwrap().source, VId(2));
        assert_eq!(new.next_edge(control).unwrap(), Some(EId(2)));
        assert!(
            new.edge(EId(2), control).unwrap().is_none(),
            "retirement must not resurrect an older row"
        );
        for source in [&mut old, &mut new] {
            assert_eq!(source.next_edge(control).unwrap(), Some(EId(u128::MAX)));
            assert!(source.edge(EId(u128::MAX), control).unwrap().is_some());
            assert_eq!(source.next_edge(control).unwrap(), None);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Same-invocation incumbent: the former root successor and uncached history
/// lookup, with identical restriction boundaries, work controls and row checks.
/// Loading, recovery and index construction stay outside the measured scans.
fn checked_scan<const CURSOR: bool>(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    as_of: CommitSeq,
    expected: &[EdgeImage],
) -> (usize, usize) {
    let mut source = view.edge_scan_source(cx, as_of).unwrap();
    let mut after = None;
    let (mut work, mut scratch) = (0, 0);
    let mut control = |event| {
        match event {
            GlaExecutionEvent::Work => work += 1,
            GlaExecutionEvent::ScratchEntry => scratch += 1,
            _ => panic!("the source cannot admit logical rows"),
        }
        Ok::<_, ()>(())
    };
    let mut rows = 0;
    loop {
        let next = if CURSOR {
            source.next_edge(&mut control).unwrap()
        } else {
            let next = cx
                .with_restriction(|| {
                    view.snapshot
                        .adjacency_index()
                        .next_identity(after, &mut || control(GlaExecutionEvent::Work))
                })
                .unwrap();
            if let Some(eid) = next {
                after = Some(eid);
            }
            next
        };
        let Some(eid) = next else { break };
        let row = if CURSOR {
            source.edge(eid, &mut control).unwrap()
        } else {
            edge_from_view(view, cx, as_of, eid, &mut control).unwrap()
        };
        if let Some(row) = row {
            let expected = &expected[rows];
            assert_eq!(
                std::hint::black_box((eid, row.source, row.relation, row.target, row.properties)),
                (
                    expected.0,
                    expected.1,
                    expected.2,
                    expected.3,
                    expected.4.as_slice()
                ),
            );
            rows += 1;
        }
    }
    assert_eq!(rows, expected.len());
    (work, scratch)
}

#[test]
#[ignore = "paired resident edge-source benchmark; creates and retains a real durable database"]
fn paired_sequential_edge_source_benchmark() {
    use std::time::Instant;

    const EDGES: u128 = 32_768;
    const ROUNDS: usize = 8;
    const PAIRS: usize = 5;
    let runtime = crate::runtime_builder().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let commit = contexts.commit();
    let keys = || {
        crate::DatabaseKeys::new(
            [0xc1; 32],
            DatabaseSecurityNamespaceId([0xc2; 32]),
            [0xc3; 32],
        )
    };
    let directory = std::env::temp_dir().join(format!(
        "fgdb-sequential-edge-source-{}",
        std::process::id()
    ));
    let mut db = runtime
        .block_on(Database::create(&commit, &directory, keys()))
        .unwrap();
    let relation = RelationId(7);
    let property = PropertyKeyId(9);
    let identity = |id: u128| EId((id << 64) | (id * 2));
    let mut create = crate::WriteBatch::new(relation);
    for vid in 0..128 {
        create.create_vertex(VId(vid), vec![], vec![]);
    }
    for id in 0..EDGES {
        create.add_edge(
            identity(id),
            VId(id % 128),
            VId((id * 7 + 1) % 128),
            vec![(property, CanonicalScalar::Int((id % 97) as i64))],
        );
    }
    runtime.block_on(db.write(&commit, create)).unwrap();
    drop(db);
    let mut db = runtime
        .block_on(Database::open(&commit, &directory, keys()))
        .unwrap();
    let flat = db.read_session().unwrap();
    assert_eq!(
        flat.snapshot.adjacency_index().base.ids.len(),
        EDGES as usize
    );
    assert_eq!(flat.snapshot.adjacency_index().histories.len(), 0);

    let mut changes = crate::WriteBatch::new(relation);
    for id in (0..EDGES).step_by(16) {
        changes.set_edge_property(identity(id), property, Some(CanonicalScalar::Int(101)));
        changes.delete_edge(identity(id + 8));
        changes.add_edge(
            EId(identity(id).0 + 1),
            VId(id % 128),
            VId((id + 3) % 128),
            vec![(property, CanonicalScalar::Int(103))],
        );
    }
    runtime.block_on(db.write(&commit, changes)).unwrap();
    let mixed = db.read_session().unwrap();
    assert!(mixed.snapshot.adjacency_index().histories.len() > 0);
    drop(db);

    for (name, view, cut) in [
        ("flat", &flat, flat.frontier()),
        ("overlay-current", &mixed, mixed.frontier()),
        ("overlay-historical", &mixed, flat.frontier()),
    ] {
        let mut expected: Vec<_> = view
            .edges_at(cut)
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.entry.eid,
                    row.entry.src,
                    row.entry.relation,
                    row.entry.dst,
                    row.props,
                )
            })
            .collect();
        expected.sort_by_key(|row| row.0);
        let incumbent_work = checked_scan::<false>(view, &cx, cut, &expected);
        let cursor_work = checked_scan::<true>(view, &cx, cut, &expected);
        let mut incumbent_ns = Vec::new();
        let mut cursor_ns = Vec::new();
        for pair in 0..PAIRS {
            // Alternate who runs first; each timed path consumes and checks
            // all the same rows while its incumbent remains live here.
            for optimized in [pair % 2 == 0, pair % 2 != 0] {
                let start = Instant::now();
                for _ in 0..ROUNDS {
                    let observed = if optimized {
                        checked_scan::<true>(view, &cx, cut, &expected)
                    } else {
                        checked_scan::<false>(view, &cx, cut, &expected)
                    };
                    std::hint::black_box(observed);
                }
                let elapsed = start.elapsed().as_nanos() / ROUNDS as u128;
                if optimized {
                    cursor_ns.push(elapsed);
                } else {
                    incumbent_ns.push(elapsed);
                }
            }
        }
        println!(
            "edge-source-pair case={name} rows={} cut={} rounds={ROUNDS} incumbent_ns={incumbent_ns:?} cursor_ns={cursor_ns:?} incumbent_work={} cursor_work={} cursor_scratch={} source=durable-reopened-and-committed timing=resident-source-scan empirical_gate_activated=false directory={}",
            expected.len(),
            cut.0,
            incumbent_work.0,
            cursor_work.0,
            cursor_work.1,
            directory.display(),
        );
    }
}
