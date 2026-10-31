//! Returned routes over real authenticated cold histories and external sort.
//! Expected paths are literal edge/vertex sequences, independently of either
//! executor. Native Database::query supplies a second complete-result check.

use super::*;
use crate::QueryResult;
use fgdb_gql::GraphAggregateValue;
use fgdb_gql::algebra::GraphPath;

const A: VId = VId(0);
const B: VId = VId(1 << 100);
const C: VId = VId(u128::MAX - 1);
const ISOLATE: VId = VId(u128::MAX);
const AB: EId = EId(0);
const PARALLEL: EId = EId((1 << 100) + 1);
const BC: EId = EId(u128::MAX - 3);
const CA: EId = EId(u128::MAX - 2);
const OTHER: EId = EId(u128::MAX - 1);
const LOOP: EId = EId(u128::MAX);
const CAPTURE: &str = "RETURN ALL p, length(p) AS n, nodes(p) AS vs, relationships(p) AS es";

async fn graph(cx: &CommitCx) -> (Database<MemVfs>, MemVfs) {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (vid, value) in [(A, 1), (B, 2), (C, 3), (ISOLATE, 4)] {
        batch.create_vertex(
            vid,
            vec![],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    for (eid, from, to) in [
        (AB, A, B),
        (PARALLEL, A, B),
        (BC, B, C),
        (CA, C, A),
        (LOOP, B, B),
    ] {
        batch.add_edge(eid, from, to, vec![]);
    }
    assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq(1));
    // A second relation distinguishes an Any-relation source from a source
    // that accidentally routes all walks through the named R relation.
    let mut other = WriteBatch::new(RelationId(2));
    other.add_edge(OTHER, A, C, vec![]);
    assert_eq!(db.write(cx, other).await.unwrap(), CommitSeq(2));
    (db, vfs)
}

fn row_bytes(rows: Vec<Vec<GraphValue>>) -> Vec<Vec<u8>> {
    rows.into_iter()
        .map(|values| {
            GraphValueRow::from_owned_values(values)
                .canonical_bytes()
                .unwrap()
        })
        .collect()
}

fn capture(start: VId, steps: &[(EId, VId)]) -> Vec<GraphValue> {
    vec![
        GraphValue::Path(GraphPath::new(start, steps.into())),
        scalar(steps.len() as i64),
        GraphValue::Vertices(
            std::iter::once(start)
                .chain(steps.iter().map(|step| step.1))
                .collect(),
        ),
        GraphValue::Edges(steps.iter().map(|step| step.0).collect()),
    ]
}

fn paths(routes: &[(VId, &[(EId, VId)])]) -> Vec<Vec<u8>> {
    let mut rows: Vec<_> = routes
        .iter()
        .map(|(start, steps)| capture(*start, steps))
        .collect();
    rows.sort();
    row_bytes(rows)
}

fn native(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> Vec<Vec<u8>> {
    let result = db
        .query(cx, text, &GqlParameters::new(), resolve, policy(1_000))
        .unwrap_or_else(|error| panic!("native {text}: {error}"));
    let QueryResult::Rows { rows, .. } = result else {
        panic!("read query returned a write receipt");
    };
    row_bytes(
        rows.into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| match value {
                        GraphAggregateValue::Value(value) => value,
                        _ => panic!("route projection returned an aggregate cell"),
                    })
                    .collect()
            })
            .collect(),
    )
}

async fn spooled(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    text: &str,
    expected: &[Vec<u8>],
    cut: u64,
    run_rows: usize,
) {
    let baseline = view.memory_pool().used();
    let spill_pool = MemoryPool::new(2_000_000, 0).unwrap();
    let prepared = prepare(text);
    let (mut scratch, _) = file(cx, &spill_pool).await;
    let (mut destination, _) = file(cx, &spill_pool).await;
    let (mut auxiliary, _) = file(cx, &spill_pool).await;
    let (spool, _) = prepared
        .spool_in_view(
            view,
            cx,
            policy(expected.len() as u64),
            &mut scratch,
            &mut destination,
            &mut auxiliary,
            2,
            32,
            run_rows,
            256,
            31,
            8192,
            200,
            WORK,
        )
        .await
        .unwrap_or_else(|error| panic!("buffered {text}, run_rows={run_rows}: {error}"));
    assert_eq!(spool.snapshot_seq(), CommitSeq(cut), "{text}");
    assert_eq!(spool.kind(), ScanKind::Edge);
    assert_eq!(spool.row_count(), expected.len() as u64, "{text}");
    assert_eq!(
        contents(&spool, &mut destination, cx).await,
        expected,
        "{text}"
    );
    assert_eq!(view.memory_pool().used(), baseline, "{text}");
    assert_eq!(spill_pool.used(), 0, "{text}");
}

async fn case(
    db: &Database<MemVfs>,
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    text: &str,
    expected: Vec<Vec<u8>>,
    cut: u64,
) {
    assert_eq!(native(db, cx, text), expected, "native {text}");
    for run_rows in [1, 3] {
        spooled(view, cx, text, &expected, cut, run_rows).await;
    }
}

#[test]
fn cold_returned_paths_preserve_modes_real_edge_identity_and_zero_hop_isolates() {
    let ((), report) = run_async_under_lab(0xc01d_3801, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, vfs) = graph(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;

        let trail = paths(&[
            (A, &[(AB, B), (BC, C), (CA, A)]),
            (A, &[(AB, B), (LOOP, B), (BC, C)]),
            (A, &[(PARALLEL, B), (BC, C), (CA, A)]),
            (A, &[(PARALLEL, B), (LOOP, B), (BC, C)]),
        ]);
        let repeatable = paths(&[
            (A, &[(AB, B), (BC, C), (CA, A)]),
            (A, &[(AB, B), (LOOP, B), (BC, C)]),
            (A, &[(AB, B), (LOOP, B), (LOOP, B)]),
            (A, &[(PARALLEL, B), (BC, C), (CA, A)]),
            (A, &[(PARALLEL, B), (LOOP, B), (BC, C)]),
            (A, &[(PARALLEL, B), (LOOP, B), (LOOP, B)]),
        ]);
        let simple = paths(&[
            (A, &[(AB, B), (BC, C), (CA, A)]),
            (A, &[(PARALLEL, B), (BC, C), (CA, A)]),
        ]);
        for (match_mode, mode, expected) in [
            ("", "WALK", trail.clone()),
            ("REPEATABLE ELEMENTS ", "WALK", repeatable),
            ("", "TRAIL", trail.clone()),
            ("REPEATABLE ELEMENTS ", "TRAIL", trail),
            ("", "ACYCLIC", vec![]),
            ("REPEATABLE ELEMENTS ", "ACYCLIC", vec![]),
            ("", "SIMPLE", simple.clone()),
            ("REPEATABLE ELEMENTS ", "SIMPLE", simple),
        ] {
            let text = format!("MATCH {match_mode}p = {mode} (a {{p:1}})-[:R*3..3]->(b) {CAPTURE}");
            case(&db, &mut view, &cx, &text, expected, 2).await;
        }
        // Closing a SIMPLE path is terminal even when closure is below the
        // requested minimum; it cannot continue around the cycle again.
        case(
            &db,
            &mut view,
            &cx,
            &format!("MATCH REPEATABLE ELEMENTS p = SIMPLE (a {{p:1}})-[:R*4..4]->(b) {CAPTURE}"),
            vec![],
            2,
        )
        .await;
        for mode in ["WALK", "TRAIL", "ACYCLIC", "SIMPLE"] {
            case(
                &db,
                &mut view,
                &cx,
                &format!("MATCH p = {mode} (a)-[:R*0..0]->(b) {CAPTURE}"),
                paths(&[(A, &[]), (B, &[]), (C, &[]), (ISOLATE, &[])]),
                2,
            )
            .await;
        }
        // DifferentEdges is independent from SIMPLE: a reverse traversal of
        // the same physical edge may close a repeatable SIMPLE route only.
        for (match_mode, expected) in [
            (
                "",
                paths(&[
                    (A, &[(AB, B), (PARALLEL, A)]),
                    (A, &[(PARALLEL, B), (AB, A)]),
                ]),
            ),
            (
                "REPEATABLE ELEMENTS ",
                paths(&[
                    (A, &[(AB, B), (AB, A)]),
                    (A, &[(AB, B), (PARALLEL, A)]),
                    (A, &[(PARALLEL, B), (AB, A)]),
                    (A, &[(PARALLEL, B), (PARALLEL, A)]),
                    (A, &[(CA, C), (CA, A)]),
                ]),
            ),
        ] {
            case(
                &db,
                &mut view,
                &cx,
                &format!(
                    "MATCH {match_mode}p = SIMPLE (a {{p:1}})-[:R*2..2]-(b) WHERE a=b {CAPTURE}"
                ),
                expected,
                2,
            )
            .await;
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_captured_directions_and_any_relation_keep_parallel_routes_and_one_self_loop() {
    let ((), report) = run_async_under_lab(0xc01d_3802, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, vfs) = graph(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        for (atom, expected) in [
            (
                "(a)-[:R*1..1]->(b)",
                paths(&[
                    (A, &[(AB, B)]),
                    (A, &[(PARALLEL, B)]),
                    (B, &[(BC, C)]),
                    (C, &[(CA, A)]),
                    (B, &[(LOOP, B)]),
                ]),
            ),
            (
                "(a)<-[:R*1..1]-(b)",
                paths(&[
                    (B, &[(AB, A)]),
                    (B, &[(PARALLEL, A)]),
                    (C, &[(BC, B)]),
                    (A, &[(CA, C)]),
                    (B, &[(LOOP, B)]),
                ]),
            ),
            (
                "(a)-[:R*1..1]-(b)",
                paths(&[
                    (A, &[(AB, B)]),
                    (B, &[(AB, A)]),
                    (A, &[(PARALLEL, B)]),
                    (B, &[(PARALLEL, A)]),
                    (B, &[(BC, C)]),
                    (C, &[(BC, B)]),
                    (C, &[(CA, A)]),
                    (A, &[(CA, C)]),
                    (B, &[(LOOP, B)]),
                ]),
            ),
            (
                "(a)-[*1..1]->(b)",
                paths(&[
                    (A, &[(AB, B)]),
                    (A, &[(PARALLEL, B)]),
                    (B, &[(BC, C)]),
                    (C, &[(CA, A)]),
                    (B, &[(LOOP, B)]),
                    (A, &[(OTHER, C)]),
                ]),
            ),
        ] {
            case(
                &db,
                &mut view,
                &cx,
                &format!("MATCH p = WALK {atom} {CAPTURE}"),
                expected,
                2,
            )
            .await;
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_returned_paths_pin_history_preserve_transit_and_compose_external_windows() {
    let ((), report) = run_async_under_lab(0xc01d_3803, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (mut db, vfs) = graph(&contexts.commit()).await;
        let pinned_pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut pinned = view(&contexts.commit(), &vfs, &pinned_pool).await;
        assert_eq!(pinned.frontier(), CommitSeq(2));
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(C, PropertyKeyId(1), Some(CanonicalScalar::Int(30)));
        update.add_edge(EId(17), C, ISOLATE, vec![]);
        assert_eq!(
            db.write(&contexts.commit(), update).await.unwrap(),
            CommitSeq(3)
        );
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut latest = view(&contexts.commit(), &vfs, &pool).await;
        assert_eq!(latest.frontier(), CommitSeq(3));

        for cut in [2, 3] {
            let end = if cut == 2 { 3 } else { 30 };
            let mut rows = vec![
                capture(A, &[(AB, B), (BC, C)]),
                capture(A, &[(PARALLEL, B), (BC, C)]),
            ];
            for row in &mut rows {
                row.extend([scalar(1), scalar(end)]);
            }
            rows.sort();
            let expected = row_bytes(rows);
            let head = format!("MATCH p = WALK (a {{p:1}})-[:R*0..2]->(b {{p:{end}}})");
            let tail =
                format!("WHERE length(p)>=2 {CAPTURE}, a.p AS start_value, b.p AS end_value");
            // B fails the terminal property predicate; it must still be
            // traversed to reach the accepted C endpoint at depth two.
            let text = format!("{head} FOR SYSTEM_TIME AS OF SEQ {cut} {tail}");
            case(&db, &mut latest, &cx, &text, expected.clone(), cut).await;
            if cut == 2 {
                let implicit = format!("{head} {tail}");
                for run_rows in [1, 3] {
                    spooled(&mut pinned, &cx, &implicit, &expected, 2, run_rows).await;
                }
            }

            let head = format!(
                "MATCH p = WALK (a {{p:1}})-[:R*0..2]->(b) FOR SYSTEM_TIME AS OF SEQ {cut}"
            );
            for (tail, expected) in [
                (
                    "RETURN b.p AS value ORDER BY length(p) DESC,b.p DESC SKIP 1 LIMIT 4",
                    frames(vec![scalar(end), scalar(2), scalar(2), scalar(2)]),
                ),
                (
                    "RETURN DISTINCT b.p AS value ORDER BY value DESC SKIP 1 LIMIT 1",
                    frames(vec![scalar(2)]),
                ),
                (
                    "WITH length(p) AS hops,b.p AS value ORDER BY hops DESC,value DESC SKIP 1 LIMIT 4 RETURN hops*10+value AS value ORDER BY value DESC",
                    frames(vec![scalar(end + 20), scalar(22), scalar(22), scalar(12)]),
                ),
                (
                    "WITH p,length(p) AS hops ORDER BY hops DESC,p SKIP 1 LIMIT 2 RETURN p ORDER BY p",
                    row_bytes(vec![
                        vec![GraphValue::Path(GraphPath::new(
                            A,
                            vec![(AB, B), (LOOP, B)].into(),
                        ))],
                        vec![GraphValue::Path(GraphPath::new(
                            A,
                            vec![(PARALLEL, B), (BC, C)].into(),
                        ))],
                    ]),
                ),
            ] {
                case(
                    &db,
                    &mut latest,
                    &cx,
                    &format!("{head} {tail}"),
                    expected,
                    cut,
                )
                .await;
            }
        }
        // The original isolated root becomes reachable only in the new
        // generation. Both cuts use the same latest cold view and real history.
        for (cut, routes) in [
            (2, vec![]),
            (
                3,
                paths(&[
                    (A, &[(AB, B), (BC, C), (EId(17), ISOLATE)]),
                    (A, &[(PARALLEL, B), (BC, C), (EId(17), ISOLATE)]),
                ]),
            ),
        ] {
            case(&db, &mut latest, &cx, &format!("MATCH p = WALK (a {{p:1}})-[:R*3..3]->(b {{p:4}}) FOR SYSTEM_TIME AS OF SEQ {cut} {CAPTURE}"), routes, cut).await;
        }
        drop(pinned);
        drop(latest);
        assert_eq!(pinned_pool.used(), 0);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_returned_paths_keep_complete_intake_errors_and_input_quota_before_zero_pages() {
    let ((), report) = run_async_under_lab(0xc01d_3804, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, vfs) = graph(&contexts.commit()).await;
        let source_pool = MemoryPool::new(32_000_000, 0).unwrap();
        let spill_pool = MemoryPool::new(2_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for run_rows in [1, 3] {
            for (limit, inputs, success) in
                [(1, 7, true), (1, 6, false), (0, 7, true), (0, 6, false)]
            {
                let text = format!(
                    "MATCH p = WALK (a {{p:1}})-[:R*0..2]->(b) RETURN b.p AS value ORDER BY value DESC LIMIT {limit}"
                );
                let expected = if limit == 0 {
                    vec![]
                } else {
                    frames(vec![scalar(3)])
                };
                assert_eq!(native(&db, &cx, &text), expected);
                let prepared = prepare(&text);
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                let result = prepared
                    .spool_in_view(
                        &mut view,
                        &cx,
                        policy(limit),
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        2,
                        32,
                        run_rows,
                        256,
                        31,
                        8192,
                        inputs,
                        WORK,
                    )
                    .await;
                if success {
                    let (spool, _) = result.unwrap();
                    assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
                } else {
                    assert!(
                        matches!(result, Err(NativeSpoolError::BufferedExecute(error)) if matches!(*error, GqlQueryError::Rows(_))),
                        "{text}"
                    );
                    assert_eq!(destination.stats().published_runs, 0);
                }
                assert_eq!(source_pool.used(), baseline);
                assert_eq!(spill_pool.used(), 0);
            }
            for limit in [0, 1] {
                // A and B emit valid rows before the first two-hop C route
                // raises division by zero. A final empty page cannot hide it.
                let text = format!(
                    "MATCH p = WALK (a {{p:1}})-[:R*0..2]->(b) WHERE 12/(3-b.p)>0 {CAPTURE} LIMIT {limit}"
                );
                assert!(
                    db.query(&cx, &text, &GqlParameters::new(), resolve, policy(1_000))
                        .is_err()
                );
                let prepared = prepare(&text);
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, backing) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                backing.0.lock().unwrap().writes = 0;
                let result = prepared
                    .spool_in_view(
                        &mut view,
                        &cx,
                        policy(limit),
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        2,
                        32,
                        run_rows,
                        256,
                        1,
                        8192,
                        100,
                        WORK,
                    )
                    .await;
                assert!(
                    matches!(result, Err(NativeSpoolError::BufferedExecute(error)) if matches!(*error, GqlQueryError::Data(error) if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)),
                    "{text}"
                );
                assert!(
                    backing.0.lock().unwrap().writes > 0,
                    "earlier native occurrences were encoded before the failure"
                );
                assert_eq!(destination.stats().published_runs, 0);
                assert_eq!(source_pool.used(), baseline);
                assert_eq!(spill_pool.used(), 0);
            }
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pending_returned_path_spill_releases_zero_hop_roots_routes_and_output_guards() {
    let ((), report) = run_async_under_lab(0xc01d_3805, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (_, vfs) = graph(&contexts.commit()).await;
        let source_pool = MemoryPool::new(32_000_000, 0).unwrap();
        let spill_pool = MemoryPool::new(2_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for (root_value, bounds) in [(4, "0..0"), (1, "2..2")] {
            let prepared = prepare(&format!(
                "MATCH p = WALK (a {{p:{root_value}}})-[:R*{bounds}]->(b) {CAPTURE}"
            ));
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, backing) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            {
                let mut state = backing.0.lock().unwrap();
                state.pending = true;
                state.writes = 0;
            }
            let mut future = prepared.spool_in_view(
                &mut view,
                &cx,
                policy(100),
                &mut scratch,
                &mut destination,
                &mut auxiliary,
                2,
                32,
                1,
                256,
                1,
                8192,
                100,
                WORK,
            );
            std::future::poll_fn(|task| match future.as_mut().poll(task) {
                Poll::Ready(_) => panic!("the blocked destination append cannot finish"),
                Poll::Pending if backing.0.lock().unwrap().writes > 0 => Poll::Ready(()),
                Poll::Pending => Poll::Pending,
            })
            .await;
            assert!(source_pool.used() > baseline);
            assert!(spill_pool.used() > 0);
            drop(future);
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
            assert!(destination.is_poisoned());
            assert_eq!(destination.stats().published_runs, 0);
            assert!(destination.stats().reserved_bytes > 0);
            assert_eq!(scratch.stats().published_runs, 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

async fn graph_with_edge_filters(cx: &CommitCx) -> (Database<MemVfs>, MemVfs) {
    let (mut db, vfs) = graph(cx).await;
    let mut properties = WriteBatch::new(RelationId(1));
    for (eid, value) in [(AB, 7), (PARALLEL, 7), (BC, 7), (CA, 8), (OTHER, 7)] {
        properties.set_edge_property(eid, PropertyKeyId(1), Some(CanonicalScalar::Int(value)));
    }
    assert_eq!(db.write(cx, properties).await.unwrap(), CommitSeq(3));
    (db, vfs)
}

#[test]
fn cold_edge_filtered_paths_preserve_history_routes_scalar_equality_and_zero_hops() {
    let ((), report) = run_async_under_lab(0xc01d_3901, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (mut db, vfs) = graph_with_edge_filters(&contexts.commit()).await;
        let pinned_pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut pinned = view(&contexts.commit(), &vfs, &pinned_pool).await;
        assert_eq!(pinned.frontier(), CommitSeq(3));

        let mut update = WriteBatch::new(RelationId(1));
        update.set_edge_property(AB, PropertyKeyId(1), Some(CanonicalScalar::Int(8)));
        assert_eq!(
            db.write(&contexts.commit(), update).await.unwrap(),
            CommitSeq(4)
        );
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut latest = view(&contexts.commit(), &vfs, &pool).await;
        assert_eq!(latest.frontier(), CommitSeq(4));

        let before = paths(&[
            (A, &[]),
            (A, &[(AB, B)]),
            (A, &[(PARALLEL, B)]),
            (A, &[(AB, B), (BC, C)]),
            (A, &[(PARALLEL, B), (BC, C)]),
        ]);
        let after = paths(&[
            (A, &[]),
            (A, &[(PARALLEL, B)]),
            (A, &[(PARALLEL, B), (BC, C)]),
        ]);
        for mode in ["WALK", "TRAIL", "ACYCLIC", "SIMPLE"] {
            // Integer and floating literals use native numeric equality.
            // CA has a different property; LOOP has no property. Neither
            // enters a retained route merely because its endpoints match.
            for wanted in ["7", "7.0"] {
                for (cut, expected) in [(3, &before), (4, &after)] {
                    let text = format!(
                        "MATCH p = {mode} (a {{p:1}})-[:R*0..3 {{p:{wanted}}}]->(b) \
                         FOR SYSTEM_TIME AS OF SEQ {cut} {CAPTURE}"
                    );
                    case(&db, &mut latest, &cx, &text, expected.clone(), cut).await;
                }
            }
            let text =
                format!("MATCH p = {mode} (a {{p:1}})-[:R*0..3 {{p:7}}]->(b) {CAPTURE}");
            for run_rows in [1, 3] {
                spooled(&mut pinned, &cx, &text, &before, 3, run_rows).await;
            }
        }

        for (atom, expected) in [
            (
                "(a)-[:R*1..1 {p:7}]->(b)",
                paths(&[(A, &[(PARALLEL, B)]), (B, &[(BC, C)])]),
            ),
            (
                "(a)<-[:R*1..1 {p:7}]-(b)",
                paths(&[(B, &[(PARALLEL, A)]), (C, &[(BC, B)])]),
            ),
            (
                "(a)-[:R*1..1 {p:7}]-(b)",
                paths(&[
                    (A, &[(PARALLEL, B)]),
                    (B, &[(PARALLEL, A)]),
                    (B, &[(BC, C)]),
                    (C, &[(BC, B)]),
                ]),
            ),
            (
                "(a)-[*1..1 {p:7}]->(b)",
                paths(&[
                    (A, &[(PARALLEL, B)]),
                    (B, &[(BC, C)]),
                    (A, &[(OTHER, C)]),
                ]),
            ),
        ] {
            case(
                &db,
                &mut latest,
                &cx,
                &format!("MATCH p = WALK {atom} {CAPTURE}"),
                expected,
                4,
            )
            .await;
        }
        let zero = paths(&[(A, &[]), (B, &[]), (C, &[]), (ISOLATE, &[])]);
        for (atom, expected) in [
            ("(a)-[:R*0..0 {p:NULL}]->(b)", zero.clone()),
            ("(a)-[:R*0..2 {p:NULL}]->(b)", zero.clone()),
            ("(a)-[:R*1..2 {p:NULL}]->(b)", vec![]),
            ("(a)-[:R*0..2 {missing:7}]->(b)", zero.clone()),
            ("(a)-[:R*1..2 {missing:7}]->(b)", vec![]),
            ("(a)-[:R*0..2 {p:7,missing:1}]->(b)", zero),
        ] {
            case(
                &db,
                &mut latest,
                &cx,
                &format!("MATCH p = WALK {atom} {CAPTURE}"),
                expected,
                4,
            )
            .await;
        }

        // Collection operands retain their explicit preparation refusal.
        // Native eager collection filtering can raise a value-bound error on
        // an unreachable edge; a local scalar predicate makes no such claim.
        for bounds in ["0..0", "0..2"] {
            let text = format!(
                "MATCH p = WALK (a)-[:R*{bounds} {{p:[7]}}]->(b) RETURN p LIMIT 0"
            );
            let parameters = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(&text, &parameters, resolve).unwrap();
            assert!(prepared.prepare_buffered_order(&parameters).is_err());
        }
        drop(pinned);
        drop(latest);
        assert_eq!(pinned_pool.used(), 0);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_edge_filtered_paths_charge_accepted_occurrences_and_keep_reached_errors() {
    let ((), report) = run_async_under_lab(0xc01d_3902, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (mut db, vfs) = graph_with_edge_filters(&contexts.commit()).await;
        let source_pool = MemoryPool::new(32_000_000, 0).unwrap();
        let spill_pool = MemoryPool::new(2_000_000, 0).unwrap();
        let mut initial = view(&contexts.commit(), &vfs, &source_pool).await;
        let baseline = source_pool.used();

        for run_rows in [1, 3] {
            for (limit, inputs, success) in
                [(1, 5, true), (1, 4, false), (0, 5, true), (0, 4, false)]
            {
                let text = format!(
                    "MATCH p = WALK (a {{p:1}})-[:R*0..3 {{p:7}}]->(b) \
                     RETURN b.p AS value ORDER BY value DESC LIMIT {limit}"
                );
                let expected = if limit == 0 {
                    vec![]
                } else {
                    frames(vec![scalar(3)])
                };
                assert_eq!(native(&db, &cx, &text), expected);
                let prepared = prepare(&text);
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                let result = prepared
                    .spool_in_view(
                        &mut initial,
                        &cx,
                        policy(limit),
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        2,
                        32,
                        run_rows,
                        256,
                        31,
                        8192,
                        inputs,
                        WORK,
                    )
                    .await;
                if success {
                    let (spool, _) = result.unwrap();
                    assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
                } else {
                    assert!(
                        matches!(result, Err(NativeSpoolError::BufferedExecute(error)) if matches!(*error, GqlQueryError::Rows(_))),
                        "{text}"
                    );
                    assert_eq!(destination.stats().published_runs, 0);
                }
                assert_eq!(source_pool.used(), baseline);
                assert_eq!(spill_pool.used(), 0);
            }
        }

        for limit in [0, 1] {
            let text = format!(
                "MATCH p = WALK (a {{p:1}})-[:R*0..3 {{p:7}}]->(b) \
                 WHERE 12/(3-b.p)>0 {CAPTURE} LIMIT {limit}"
            );
            assert!(
                db.query(&cx, &text, &GqlParameters::new(), resolve, policy(1_000))
                    .is_err()
            );
            let prepared = prepare(&text);
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, backing) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            backing.0.lock().unwrap().writes = 0;
            let result = prepared
                .spool_in_view(
                    &mut initial,
                    &cx,
                    policy(limit),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    2,
                    32,
                    1,
                    256,
                    1,
                    8192,
                    100,
                    WORK,
                )
                .await;
            assert!(
                matches!(result, Err(NativeSpoolError::BufferedExecute(error)) if matches!(*error, GqlQueryError::Data(error) if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)),
                "{text}"
            );
            assert!(backing.0.lock().unwrap().writes > 0);
            assert_eq!(destination.stats().published_runs, 0);
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(initial);
        assert_eq!(source_pool.used(), 0);

        // Rejecting BC removes the transit edge itself. C's endpoint expression
        // is now unreachable, while A and both parallel routes to B survive.
        let mut update = WriteBatch::new(RelationId(1));
        update.set_edge_property(BC, PropertyKeyId(1), Some(CanonicalScalar::Int(8)));
        assert_eq!(
            db.write(&contexts.commit(), update).await.unwrap(),
            CommitSeq(4)
        );
        let mut latest = view(&contexts.commit(), &vfs, &source_pool).await;
        let head = "MATCH p = WALK (a {p:1})-[:R*0..3 {p:7}]->(b) WHERE 12/(3-b.p)>0";
        case(
            &db,
            &mut latest,
            &cx,
            &format!("{head} {CAPTURE}"),
            paths(&[(A, &[]), (A, &[(AB, B)]), (A, &[(PARALLEL, B)])]),
            4,
        )
        .await;
        case(
            &db,
            &mut latest,
            &cx,
            &format!("{head} {CAPTURE} LIMIT 0"),
            vec![],
            4,
        )
        .await;
        drop(latest);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(spill_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
