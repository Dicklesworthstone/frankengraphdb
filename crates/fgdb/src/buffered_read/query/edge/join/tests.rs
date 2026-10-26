use super::*;
use crate::{BufferLimits, BufferedReadLimits, Database, DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::edge_stream::EdgeScanState;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};

const R: RelationId = RelationId(7);
const P: PropertyKeyId = PropertyKeyId(9);
const IDS: [VId; 3] = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
const EDGES: [(u128, usize, usize, i64); 5] = [
    (0, 0, 1, 7),
    (1, 0, 1, 8),
    (2, 1, 2, 9),
    (3, 2, 0, 10),
    (u128::MAX, 1, 1, 11),
];

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}
fn limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 64 * 1024,
        max_source_bytes: 4 * 1024 * 1024,
        max_blocks: 512,
        max_vertex_patches: 512,
        max_work: 10_000_000,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn pool() -> MemoryPool {
    MemoryPool::new(64 * 1024 * 1024, 0).unwrap()
}
fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(8))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "missing") => Some(GraphSymbol::Property(PropertyKeyId(10))),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
async fn history(cx: &CommitCx) -> (MemVfs, crate::EmbeddedReadView) {
    let vfs = MemVfs::new().unwrap();
    let path = vfs.database_dir();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), &path, keys())
        .await
        .unwrap();
    let mut first = WriteBatch::new(R);
    for (at, vid) in IDS.iter().enumerate() {
        first.create_vertex(*vid, vec![], vec![(P, CanonicalScalar::Int(at as i64 + 1))]);
    }
    for (eid, from, to, value) in EDGES {
        first.add_edge(
            EId(eid),
            IDS[from],
            IDS[to],
            vec![(P, CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(db.write(cx, first).await.unwrap(), CommitSeq(1));
    let mut second = WriteBatch::new(R);
    second.set_vertex_property(IDS[0], P, Some(CanonicalScalar::Int(21)));
    second.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(17)));
    second.delete_edge(EId(2));
    assert_eq!(db.write(cx, second).await.unwrap(), CommitSeq(2));
    let mut third = WriteBatch::new(RelationId(8));
    third.add_edge(EId(4), IDS[1], IDS[0], vec![(P, CanonicalScalar::Int(20))]);
    assert_eq!(db.write(cx, third).await.unwrap(), CommitSeq(3));
    (vfs, db.read_session().unwrap())
}
fn atom(edge: &str, end: &str, direction: usize, any: bool) -> String {
    let relation = if any { "" } else { ":R" };
    match direction {
        0 => format!("-[{edge}{relation}]->({end})"),
        1 => format!("<-[{edge}{relation}]-({end})"),
        _ => format!("-[{edge}{relation}]-({end})"),
    }
}
fn statement(
    shape: usize,
    dirs: [usize; 2],
    any: bool,
    filter: bool,
    skip: usize,
    count: usize,
) -> String {
    let head = format!("MATCH (a){}", atom("r", "b", dirs[0], any));
    let tail = match shape {
        1 => format!(", (a){}", atom("s", "c", dirs[1], any)),
        2 => atom("s", "a", dirs[1], any),
        _ => atom("s", "c", dirs[1], any),
    };
    let end = if shape == 2 { "a AS c" } else { "c" };
    let cp = if shape == 2 { "a.p" } else { "c.p" };
    let predicate = if filter {
        "WHERE r.p >= 8 AND s.p > a.p"
    } else {
        ""
    };
    let quantifier = if filter { "DISTINCT" } else { "ALL" };
    format!(
        "{head}{tail} {predicate} RETURN {quantifier} r,a,s,b,{end},r.p AS rp,s.p AS sp,a.p AS ap,{cp} AS cp SKIP {skip} LIMIT {count}"
    )
}

// Independent finite relation product, not the storage/index/GLA implementation.
// Expected histories, field values and directions are computed from literals.
fn expected(
    cut: u64,
    shape: usize,
    dirs: [usize; 2],
    any: bool,
    filter: bool,
    skip: usize,
    count: usize,
) -> Vec<Vec<GraphValue>> {
    if cut == 0 {
        return Vec::new();
    }
    let mut edges: Vec<_> = EDGES
        .into_iter()
        .filter(|(id, _, _, _)| cut == 1 || *id != 2)
        .collect();
    if cut >= 3 && any {
        edges.push((4, 1, 0, 20));
    }
    if cut >= 2 {
        edges.iter_mut().find(|e| e.0 == 0).unwrap().3 = 17;
    }
    let property = |vertex: usize| {
        if cut >= 2 && vertex == 0 {
            21
        } else {
            vertex as i64 + 1
        }
    };
    let orient = |from: usize, to: usize, direction| match direction {
        0 => vec![(from, to)],
        1 => vec![(to, from)],
        _ if from == to => vec![(from, to)],
        _ => vec![(from, to), (to, from)],
    };
    let mut rows = Vec::new();
    for &(r, x, y, rp) in &edges {
        for (a, b) in orient(x, y, dirs[0]) {
            for &(s, x, y, sp) in &edges {
                for (from, c) in orient(x, y, dirs[1]) {
                    // One MATCH binds r and s under DIFFERENT EDGES.
                    if r == s
                        || from != (if shape == 1 { a } else { b })
                        || (shape == 2 && c != a)
                        || (filter && (rp < 8 || sp <= property(a)))
                    {
                        continue;
                    }
                    rows.push(vec![
                        GraphValue::Edge(EId(r)),
                        GraphValue::Vertex(IDS[a]),
                        GraphValue::Edge(EId(s)),
                        GraphValue::Vertex(IDS[b]),
                        GraphValue::Vertex(IDS[c]),
                        GraphValue::Scalar(CanonicalScalar::Int(rp)),
                        GraphValue::Scalar(CanonicalScalar::Int(sp)),
                        GraphValue::Scalar(CanonicalScalar::Int(property(a))),
                        GraphValue::Scalar(CanonicalScalar::Int(property(c))),
                    ]);
                }
            }
        }
    }
    rows.sort();
    if filter {
        rows.dedup();
    }
    rows.into_iter().skip(skip).take(count).collect()
}

#[test]
fn cold_joins_match_864_independent_history_shape_orientation_and_window_cases() {
    let ((), report) = run_async_under_lab(0xc01d_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, resident) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        let mut cases = 0;
        for cut in 0..=3 {
            for shape in 0..3 {
                for left in 0..3 {
                    for right in 0..3 {
                        for any in [false, true] {
                            for filter in [false, true] {
                                for (skip, count) in [(0, 1000), (1, 3)] {
                                    let text =
                                        statement(shape, [left, right], any, filter, skip, count);
                                    let p = pattern(&text);
                                    let identity = p.plan().canonical_bytes();
                                    let want = expected(
                                        cut,
                                        shape,
                                        [left, right],
                                        any,
                                        filter,
                                        skip,
                                        count,
                                    );
                                    let synchronous: Vec<_> = resident
                                        .stream_graph_edges_governed_at(
                                            &cx,
                                            &p,
                                            CommitSeq(cut),
                                            policy(),
                                        )
                                        .unwrap()
                                        .map(|r| r.unwrap().values().to_vec())
                                        .collect();
                                    assert_eq!(synchronous, want, "resident: {text} at {cut}");
                                    let mut cursor = view
                                        .stream_graph_edge_joins_governed_at(
                                            &cx,
                                            &p,
                                            CommitSeq(cut),
                                            policy(),
                                        )
                                        .unwrap();
                                    assert_eq!(cursor.row_stats().snapshot_records, 0);
                                    let mut rows = Vec::new();
                                    while let Some(row) = cursor.next().await {
                                        rows.push(row.unwrap().values().to_vec());
                                        assert!(pool.used() <= pool.limit());
                                    }
                                    assert_eq!(rows, want, "buffered: {text} at {cut}");
                                    assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
                                    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                                    assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                                    assert!(cursor.next().await.is_none());
                                    drop(cursor);
                                    assert_eq!(pool.used(), baseline);
                                    assert_eq!(p.plan().canonical_bytes(), identity);
                                    cases += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 864);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_join_admission_demand_and_output_guards_obey_the_native_lifecycle() {
    let ((), report) = run_async_under_lab(0xc01d_2002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        let unsupported = pattern("MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN c");
        assert!(matches!(
            view.stream_graph_edge_joins_governed(&cx, &unsupported, policy()),
            Err(GqlQueryError::Source(EdgeScanError::Plan(_)))
        ));
        let zero = pattern(&statement(0, [2, 2], true, false, 0, 0));
        assert!(matches!(
            view.stream_graph_edge_joins_governed_at(&cx, &zero, CommitSeq(4), policy()),
            Err(GqlQueryError::Source(EdgeScanError::Source(
                BufferedReadError::BeyondPublication { .. }
            )))
        ));
        let mut cursor = view
            .stream_graph_edge_joins_governed(&cx, &zero, policy())
            .unwrap();
        let future = cursor.next();
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&future);
        drop(future);
        assert_eq!(cursor.state(), EdgeScanState::Open);
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        drop(cursor);
        assert_eq!(pool.used(), baseline);
        assert_eq!(view.buffer_stats().misses, 0);
        let p = pattern(&statement(0, [2, 2], true, false, 0, 1000));
        let mut cursor = view
            .stream_graph_edge_joins_governed(&cx, &p, policy())
            .unwrap();
        drop(p); // The native plan owns its definition.
        let first = cursor.next().await.unwrap().unwrap();
        assert_eq!(
            first.values(),
            expected(3, 0, [2, 2], true, false, 0, 1)[0].as_slice()
        );
        let used = cursor.row_stats();
        cursor.close();
        assert_eq!(cursor.state(), EdgeScanState::Closed);
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.row_stats(), used);
        drop(cursor);
        drop(view);
        assert!(
            pool.used() > 0,
            "the result, not routing or traversal, owns the remaining charge"
        );
        drop(first);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_root_nested_and_output_quotas_are_cumulative_and_memory_failure_never_falls_back() {
    let ((), report) = run_async_under_lab(0xc01d_2003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        let p = pattern(&statement(0, [0, 0], true, false, 0, 1000));
        let mut complete = view
            .stream_graph_edge_joins_governed(&cx, &p, policy())
            .unwrap();
        let mut rows = Vec::new();
        while let Some(row) = complete.next().await {
            rows.push(row.unwrap().values().to_vec());
        }
        assert_eq!(rows, expected(3, 0, [0, 0], true, false, 0, 1000));
        let r = complete.row_stats();
        let e = complete.evaluator_stats();
        assert!(
            r.snapshot_records > 6,
            "nested histories spend the root allowance"
        );
        assert!(r.result_rows > 1 && e.work_units > 1 && e.scratch_entries > 1);
        drop(complete);
        assert_eq!(pool.used(), baseline);
        for dimension in 0..=4 {
            let mut values = [
                r.snapshot_records,
                r.result_rows,
                e.work_units,
                e.scratch_entries,
            ];
            if dimension != 4 {
                values[dimension] -= 1;
            }
            let allowance = GqlQueryPolicy::new(values[0], values[1], values[2], values[3]);
            let mut cursor = view
                .stream_graph_edge_joins_governed(&cx, &p, allowance)
                .unwrap();
            let mut failed = false;
            while let Some(row) = cursor.next().await {
                if let Err(error) = row {
                    assert!(matches!(
                        error,
                        GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)
                    ));
                    failed = true;
                    break;
                }
            }
            assert_eq!(failed, dimension != 4);
            assert_eq!(
                cursor.state(),
                if failed {
                    EdgeScanState::Failed
                } else {
                    EdgeScanState::Exhausted
                }
            );
            assert!(cursor.next().await.is_none());
            drop(cursor);
            assert_eq!(pool.used(), baseline);
        }
        // Leave room for cursor metadata, but not a bounded decoded block.
        let hold = pool
            .reserve(&cx, pool.limit() - pool.used() - 64 * 1024)
            .unwrap();
        let held = pool.used();
        let mut cursor = view
            .stream_graph_edge_joins_governed(&cx, &p, policy())
            .unwrap();
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Source(EdgeScanError::Source(
                BufferedReadError::Buffer(fgdb_strata::tiered::buffer::BufferError::Memory(_))
            ))))
        ));
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(pool.used(), held);
        drop(hold);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_join_pin_survives_later_write_compaction_and_successor_reopen() {
    let ((), report) = run_async_under_lab(0xc01d_2004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let old_pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            &path,
            keys(),
            old_pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let p = pattern(&statement(0, [0, 0], true, false, 0, 1000));
        let mut cursor = view
            .stream_graph_edge_joins_governed(&cx, &p, policy())
            .unwrap();
        let mut actual = vec![cursor.next().await.unwrap().unwrap().values().to_vec()];
        let mut writer = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(IDS[0], P, Some(CanonicalScalar::Int(99)));
        change.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(77)));
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(4));
        writer.compact(&commit).await.unwrap();
        drop(writer);
        while let Some(row) = cursor.next().await {
            actual.push(row.unwrap().values().to_vec());
        }
        assert_eq!(actual, expected(3, 0, [0, 0], true, false, 0, 1000));
        drop(cursor);
        drop(view);
        assert_eq!(old_pool.used(), 0);
        let new_pool = pool();
        let mut reopened = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            new_pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let mut cursor = reopened
            .stream_graph_edge_joins_governed(&cx, &p, policy())
            .unwrap();
        let row = cursor.next().await.unwrap().unwrap();
        assert_eq!(row.values()[0], GraphValue::Edge(EId(0)));
        assert_eq!(
            row.values()[5],
            GraphValue::Scalar(CanonicalScalar::Int(77))
        );
        assert_eq!(
            row.values()[7],
            GraphValue::Scalar(CanonicalScalar::Int(99))
        );
        drop(cursor);
        drop(reopened);
        assert!(new_pool.used() > 0);
        drop(row);
        assert_eq!(new_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn interrupted_cold_incidence_admission_drops_routing_and_cannot_resume_a_partial_prefix() {
    let ((), report) = run_async_under_lab(0xc01d_2005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        let (mut source, checkpoint) = view
            .edge_input_source(&cx, CommitSeq(3), EdgeRelation::Any)
            .unwrap();
        let mut events = Vec::new();
        let candidate = source
            .next_incident_candidate(
                IDS[1],
                EdgeRelation::Any,
                GlaDirection::Forward,
                None,
                &mut |event| {
                    events.push(event);
                    Ok::<_, usize>(())
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(candidate.eid, EId(2));
        assert!(
            candidate.record.is_none(),
            "retired history is charged but never resurrected"
        );
        let admission = events
            .iter()
            .position(|e| matches!(e, AsyncEdgeScanEvent::Candidate(_)))
            .unwrap()
            + 1;
        let total = events.len();
        drop(candidate);
        drop(source);
        drop(checkpoint);
        assert_eq!(pool.used(), baseline);
        for stop in [1, admission, total] {
            let (mut source, checkpoint) = view
                .edge_input_source(&cx, CommitSeq(3), EdgeRelation::Any)
                .unwrap();
            let mut calls = 0;
            let result = source
                .next_incident_candidate(
                    IDS[1],
                    EdgeRelation::Any,
                    GlaDirection::Forward,
                    None,
                    &mut |_| {
                        calls += 1;
                        if calls == stop { Err(stop) } else { Ok(()) }
                    },
                )
                .await;
            assert!(matches!(result, Err(EdgeExpansionSourceError::Read(
                EdgeScanSourceError::Control(value))) if value == stop));
            assert_eq!(calls, stop);
            assert!(source.scan.is_closed());
            assert!(
                source
                    .next_candidate(EdgeRelation::Any, &mut |_| -> Result<(), usize> {
                        panic!("a failed cold source cannot be driven again");
                    })
                    .await
                    .unwrap()
                    .is_none()
            );
            drop(source);
            drop(checkpoint);
            assert_eq!(pool.used(), baseline);
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_selected_root_prunes_payload_faults_and_control_refusals_still_fuse_the_source() {
    let ((), report) = run_async_under_lab(0xc01d_2006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        let mut measurements = Vec::new();
        // The unscoped source is the predecessor's actual access path, with
        // the SAME one-relation evaluator demand and native history resolver.
        for root_relation in [EdgeRelation::One(R), EdgeRelation::Any] {
            let before = view.buffer_stats().misses;
            let (mut source, checkpoint) = view
                .edge_input_source(&cx, CommitSeq(3), root_relation)
                .unwrap();
            let mut events = Vec::new();
            let mut rows = Vec::new();
            while let Some(candidate) = source
                .next_candidate(EdgeRelation::One(R), &mut |event| {
                    events.push(event);
                    Ok::<_, usize>(())
                })
                .await
                .unwrap()
            {
                if let Some(record) = candidate.record {
                    let edge = record.image.edge();
                    rows.push((candidate.eid, edge.entry, edge.props.clone()));
                }
            }
            drop(source);
            drop(checkpoint);
            assert_eq!(pool.used(), baseline);
            measurements.push((rows, events, view.buffer_stats().misses - before));
        }
        assert_eq!(measurements[0].0, measurements[1].0);
        let candidates = |events: &[AsyncEdgeScanEvent]| {
            events
                .iter()
                .filter_map(|event| match event {
                    AsyncEdgeScanEvent::Candidate(eid) => Some(*eid),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            candidates(&measurements[0].1),
            [EId(0), EId(1), EId(2), EId(3), EId(u128::MAX)]
        );
        assert_eq!(
            candidates(&measurements[1].1),
            [EId(0), EId(1), EId(2), EId(3), EId(4), EId(u128::MAX)]
        );
        assert!(
            measurements[0].2 < measurements[1].2,
            "descriptor pruning must avoid actual payload faults"
        );

        let events = &measurements[0].1;
        let first_identity = events
            .iter()
            .position(|event| matches!(event, AsyncEdgeScanEvent::Candidate(_)))
            .unwrap()
            + 1;
        for stop in [1, first_identity, events.len()] {
            let (mut source, checkpoint) = view
                .edge_input_source(&cx, CommitSeq(3), EdgeRelation::One(R))
                .unwrap();
            let mut calls = 0;
            loop {
                let result = source
                    .next_candidate(EdgeRelation::One(R), &mut |_| {
                        calls += 1;
                        if calls == stop { Err(stop) } else { Ok(()) }
                    })
                    .await;
                match result {
                    Ok(Some(_)) => {}
                    Err(EdgeScanSourceError::Control(value)) => {
                        assert_eq!(value, stop);
                        break;
                    }
                    Ok(None) => panic!("a refused root cannot finish successfully"),
                    Err(error) => panic!("root refusal must retain its type: {error:?}"),
                }
            }
            assert_eq!(calls, stop);
            assert!(source.scan.is_closed());
            assert!(
                source
                    .next_candidate(EdgeRelation::Any, &mut |_| -> Result<(), usize> {
                        panic!("a failed selected root must not resume or widen");
                    })
                    .await
                    .unwrap()
                    .is_none()
            );
            drop(source);
            drop(checkpoint);
            assert_eq!(pool.used(), baseline);
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn typed_join_roots_keep_other_relation_expansions_and_their_exact_candidate_budgets() {
    let ((), report) = run_async_under_lab(0xc01d_2007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        for (text, quota, expected) in [
            (
                "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN r,a,s,b,c",
                8,
                vec![(0, 0, 4, 1, 0), (1, 0, 4, 1, 0), (u128::MAX, 1, 4, 1, 0)],
            ),
            (
                "MATCH (a)-[r:S]->(b)-[s:R]->(c) RETURN r,a,s,b,c",
                3,
                vec![(4, 1, 0, 0, 1), (4, 1, 1, 0, 1)],
            ),
        ] {
            let p = pattern(text);
            let bytes = p.plan().canonical_bytes();
            let mut cursor = view
                .stream_graph_edge_joins_governed(
                    &cx,
                    &p,
                    GqlQueryPolicy::new(quota, 100, 1_000_000, 1_000_000),
                )
                .unwrap();
            let mut actual = Vec::new();
            while let Some(row) = cursor.next().await {
                actual.push(row.unwrap().values().to_vec());
            }
            let expected: Vec<_> = expected
                .into_iter()
                .map(|(r, a, s, b, c)| {
                    vec![
                        GraphValue::Edge(EId(r)),
                        GraphValue::Vertex(IDS[a]),
                        GraphValue::Edge(EId(s)),
                        GraphValue::Vertex(IDS[b]),
                        GraphValue::Vertex(IDS[c]),
                    ]
                })
                .collect();
            assert_eq!(actual, expected, "{text}");
            assert_eq!(cursor.row_stats().snapshot_records, quota);
            assert_eq!(cursor.state(), EdgeScanState::Exhausted);
            drop(cursor);
            assert_eq!(pool.used(), baseline);
            assert_eq!(p.plan().canonical_bytes(), bytes);
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn buffered_sort_and_numeric_aggregate_inputs_use_their_compiled_root_selector() {
    use core::convert::Infallible;
    use fgdb_gql::spill_aggregate::AsyncSpillAggregatePlan;
    use fgdb_gql::{GraphAggregateValue, PreparedGraphAggregateText};

    let ((), report) = run_async_under_lab(0xc01d_2008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        let p = pattern("MATCH (a)-[r:R]->(b) RETURN r.p AS value ORDER BY value DESC");
        let (plan, _) = AsyncEdgeScanPlan::compile_sort_input(p.plan()).unwrap();
        let mut cursor = view
            .open_edge_input(
                &cx,
                plan,
                CommitSeq(3),
                GqlQueryPolicy::new(5, 100, 1_000_000, 1_000_000),
            )
            .unwrap();
        let mut occurrences = 0;
        while let Some(row) = cursor.next().await {
            row.unwrap();
            occurrences += 1;
        }
        assert_eq!(occurrences, 4);
        assert_eq!(cursor.row_stats().snapshot_records, 5);
        drop(cursor);
        assert_eq!(pool.used(), baseline);

        for (text, records, expected) in [
            ("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n", 5, 4),
            ("MATCH (a)-[r:S]->(b) RETURN COUNT(*) AS n", 1, 1),
            ("MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN COUNT(*) AS n", 8, 3),
            ("MATCH (a)-[r:S]->(b)-[s:R]->(c) RETURN COUNT(*) AS n", 3, 2),
        ] {
            let aggregate =
                PreparedGraphAggregateText::prepare(text, |kind, name| match (kind, name) {
                    (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
                    (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(8))),
                    _ => None,
                })
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let plan = AsyncSpillAggregatePlan::compile(&aggregate).unwrap();
            let definition = plan.definition().clone();
            let allowance = GqlQueryPolicy::new(records, 1, 1_000_000, 1_000_000);
            macro_rules! consume {
                ($cursor:expr) => {{
                    let mut cursor = $cursor.unwrap();
                    let mut state = definition
                        .new_state(&mut |event| cursor.charge::<Infallible, Cancel>(event))
                        .unwrap();
                    while let Some(row) = cursor
                        .next_input(&mut |_, _| Ok::<_, BufferedReadError>(()))
                        .await
                        .unwrap()
                    {
                        definition
                            .update(&mut state, &row, &mut |event| {
                                cursor.charge::<Infallible, Cancel>(event)
                            })
                            .unwrap();
                    }
                    let result = definition
                        .finish(vec![], state, &mut |event| {
                            cursor.charge::<Infallible, Cancel>(event)
                        })
                        .unwrap();
                    assert_eq!(
                        result.values(),
                        &[GraphAggregateValue::Count(expected)],
                        "{text}"
                    );
                    assert_eq!(cursor.row_stats().snapshot_records, records, "{text}");
                    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                    cursor.finish_result::<Infallible, Cancel>().unwrap();
                }};
            }
            match plan {
                AsyncSpillAggregatePlan::Edge(plan) => {
                    consume!(view.open_edge_aggregate_input(&cx, plan, CommitSeq(3), allowance))
                }
                AsyncSpillAggregatePlan::Join(plan) => consume!(
                    view.open_edge_join_aggregate_input(&cx, plan, CommitSeq(3), allowance)
                ),
                other => panic!("expected an edge source: {other:?}"),
            }
            assert_eq!(pool.used(), baseline);
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
