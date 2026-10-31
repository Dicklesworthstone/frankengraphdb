//! Correlated probes over real committed histories and the cold Strata reader.
//! Literal graph products below are test oracles, never production sources.

// An integration test's children resolve beside it, not under its own directory.
#[path = "buffered_probe_queries/spill.rs"]
mod spill;

use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys, MemVfs, MemoryPool,
    WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};

const R: RelationId = RelationId(7);
const S: RelationId = RelationId(8);
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
        [0xc4; 32],
        DatabaseSecurityNamespaceId([0xc5; 32]),
        [0xc6; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
async fn seed(contexts: &PurposeContexts, vfs: &MemVfs) -> Database<MemVfs> {
    let mut db =
        Database::create_with_vfs(&contexts.commit(), vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
    let mut batch = WriteBatch::new(R);
    for (at, id) in IDS.iter().enumerate() {
        batch.create_vertex(*id, vec![], vec![(P, CanonicalScalar::Int(at as i64 + 1))]);
    }
    for (id, from, to, p) in EDGES {
        batch.add_edge(
            EId(id),
            IDS[from],
            IDS[to],
            vec![(P, CanonicalScalar::Int(p))],
        );
    }
    assert_eq!(
        db.write(&contexts.commit(), batch).await.unwrap(),
        CommitSeq(1)
    );
    db
}
async fn cold(
    contexts: &PurposeContexts,
    vfs: &MemVfs,
    pool: MemoryPool,
) -> BufferedReadView<MemVfs> {
    let limits = BufferedReadLimits {
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
    };
    Database::open_buffered_read_view_with_vfs(
        &contexts.commit(),
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        pool,
        limits,
    )
    .await
    .unwrap()
}
fn memory() -> MemoryPool {
    MemoryPool::new(64 * 1024 * 1024, 0).unwrap()
}
async fn collect(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    plan: &PreparedGraphPattern<GraphValueRow>,
    cut: CommitSeq,
) -> Vec<Vec<GraphValue>> {
    let mut cursor = view
        .stream_graph_edge_joins_governed_at(cx, plan, cut, policy())
        .unwrap();
    assert_eq!(cursor.snapshot_seq(), cut);
    let mut rows = Vec::new();
    while let Some(result) = cursor.next().await {
        let (row, charge) = result.unwrap().into_parts();
        rows.push(row.values().to_vec()); // The oracle's copy is application-owned.
        drop(row);
        drop(charge);
    }
    assert!(cursor.next().await.is_none());
    rows
}
fn atom(edge: &str, end: &str, direction: usize) -> String {
    match direction {
        0 => format!("-[{edge}:R]->({end})"),
        1 => format!("<-[{edge}:R]-({end})"),
        _ => format!("-[{edge}:R]-({end})"),
    }
}
fn orient(from: usize, to: usize, direction: usize) -> Vec<(usize, usize)> {
    match direction {
        0 => vec![(from, to)],
        1 => vec![(to, from)],
        _ if from == to => vec![(from, to)],
        _ => vec![(from, to), (to, from)],
    }
}
fn text(directions: [usize; 2], two_hops: bool, anti: bool, repeatable: bool) -> String {
    let root = atom("r", "b", directions[0]);
    let child = atom("s", "c", directions[1]);
    let tail = if two_hops {
        atom("t", "a", directions[1])
    } else {
        String::new()
    };
    let mode = if repeatable {
        "REPEATABLE ELEMENTS "
    } else {
        ""
    };
    let negation = if anti { "NOT " } else { "" };
    format!(
        "MATCH (a){root} WHERE {negation}EXISTS {{ MATCH {mode}(b){child}{tail} \
        WHERE s.p>=9 }} RETURN r,a,b,r.p AS rp,a.p AS ap"
    )
}
fn expected(
    cut: u64,
    directions: [usize; 2],
    two_hops: bool,
    anti: bool,
    repeatable: bool,
) -> Vec<Vec<GraphValue>> {
    if cut == 0 {
        return Vec::new();
    }
    let edges: Vec<_> = EDGES
        .into_iter()
        .filter(|(id, _, _, _)| cut < 2 || *id != 2)
        .map(|(id, a, b, p)| (id, a, b, if cut >= 2 && id == 0 { 17 } else { p }))
        .collect();
    let mut rows = Vec::new();
    for &(r, x, y, rp) in &edges {
        for (a, b) in orient(x, y, directions[0]) {
            // Enumerate the complete local product. The root's EId is not a
            // member of this separate clause's DIFFERENT EDGES domain.
            let mut witnesses = 0;
            for &(s, x, y, sp) in &edges {
                for (from, c) in orient(x, y, directions[1]) {
                    if from != b || sp < 9 {
                        continue;
                    }
                    if !two_hops {
                        witnesses += 1;
                        continue;
                    }
                    for &(t, x, y, _) in &edges {
                        if !repeatable && t == s {
                            continue;
                        }
                        for (from, end) in orient(x, y, directions[1]) {
                            if from == c && end == a {
                                witnesses += 1;
                            }
                        }
                    }
                }
            }
            if (witnesses != 0) == anti {
                continue;
            }
            let ap = if cut >= 2 && a == 0 { 21 } else { a as i64 + 1 };
            rows.push(vec![
                GraphValue::Edge(EId(r)),
                GraphValue::Vertex(IDS[a]),
                GraphValue::Vertex(IDS[b]),
                GraphValue::Scalar(CanonicalScalar::Int(rp)),
                GraphValue::Scalar(CanonicalScalar::Int(ap)),
            ]);
        }
    }
    rows.sort();
    rows
}

#[test]
fn cold_correlated_probes_match_literal_products_and_resident_historical_queries() {
    let ((), report) = run_async_under_lab(0xe6_51, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let pinned_pool = memory();
        let mut pinned = cold(&contexts, &vfs, pinned_pool.clone()).await;
        let mut second = WriteBatch::new(R);
        second.set_vertex_property(IDS[0], P, Some(CanonicalScalar::Int(21)));
        second.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(17)));
        second.delete_edge(EId(2));
        db.write(&contexts.commit(), second).await.unwrap();
        let mut third = WriteBatch::new(S);
        third.add_edge(EId(4), IDS[1], IDS[0], vec![(P, CanonicalScalar::Int(20))]);
        db.write(&contexts.commit(), third).await.unwrap();
        let mut view = cold(&contexts, &vfs, memory()).await;
        let mut cases = 0;
        for cut in 0..=3 {
            for first in 0..3 {
                for child in 0..3 {
                    for two_hops in [false, true] {
                        for anti in [false, true] {
                            for repeatable in [false, true] {
                                let text = text([first, child], two_hops, anti, repeatable);
                                let plan = prepare(&text);
                                let expected =
                                    expected(cut, [first, child], two_hops, anti, repeatable);
                                let resident = db
                                    .execute_graph_pattern_governed_at(
                                        &cx,
                                        &plan,
                                        CommitSeq(cut),
                                        policy(),
                                    )
                                    .unwrap();
                                assert_eq!(
                                    resident
                                        .value
                                        .iter()
                                        .map(|row| row.values().to_vec())
                                        .collect::<Vec<_>>(),
                                    expected,
                                    "resident {text} at {cut}"
                                );
                                assert_eq!(
                                    collect(&mut view, &cx, &plan, CommitSeq(cut)).await,
                                    expected,
                                    "cold {text} at {cut}"
                                );
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 288);
        let statement = text([2, 2], true, false, false);
        let plan = prepare(&statement);
        let old = expected(1, [2, 2], true, false, false);
        let latest = expected(3, [2, 2], true, false, false);
        assert_eq!(collect(&mut pinned, &cx, &plan, CommitSeq(1)).await, old);
        db.compact(&contexts.commit()).await.unwrap();
        drop(db);
        let reopened =
            Database::open_with_vfs(&contexts.commit(), vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        let mut compacted = cold(&contexts, &vfs, memory()).await;
        assert_eq!(
            collect(&mut compacted, &cx, &plan, CommitSeq(3)).await,
            latest
        );
        assert_eq!(collect(&mut pinned, &cx, &plan, CommitSeq(1)).await, old);
        let paged = prepare(&format!("{statement} SKIP 1 LIMIT 2"));
        assert_eq!(
            collect(&mut compacted, &cx, &paged, CommitSeq(3)).await,
            latest.into_iter().skip(1).take(2).collect::<Vec<_>>()
        );
        assert_eq!(reopened.frontier().unwrap(), CommitSeq(3));
        drop(pinned);
        assert_eq!(pinned_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn buffered_edge_queries_seek_endpoint_patches_under_a_subquadratic_source_budget() {
    let ((), report) = run_async_under_lab(0xe6_54, |root| async move {
        const VERTICES: usize = 1024;
        const EDGES: usize = VERTICES / 2;
        const SOURCE_WORK: usize = 100_000;
        const PADDING: PropertyKeyId = PropertyKeyId(10);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        for start in (0..VERTICES).step_by(128) {
            let mut batch = WriteBatch::new(R);
            for id in start..start + 128 {
                batch.create_vertex(
                    VId(id as u128 + 1),
                    vec![],
                    vec![
                        (P, CanonicalScalar::Int(id as i64)),
                        (PADDING, CanonicalScalar::bytes(vec![0x5a; 2048]).unwrap()),
                    ],
                );
            }
            db.write(&commit, batch).await.unwrap();
        }
        let mut edges = WriteBatch::new(R);
        for id in 0..EDGES {
            edges.add_edge(
                EId(id as u128),
                VId(2 * id as u128 + 1),
                VId(2 * id as u128 + 2),
                vec![],
            );
        }
        db.write(&commit, edges).await.unwrap();
        drop(db);
        // Even without row decoding, the previous full descriptor scan for
        // both endpoints exceeds this budget. A 16 KiB patch cannot hold eight
        // rows whose scalar payload alone occupies 2048 bytes apiece.
        assert!(2 * EDGES * VERTICES.div_ceil(7) > SOURCE_WORK);
        let pool = memory();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            pool.clone(),
            BufferedReadLimits {
                max_root_bytes: 256 * 1024,
                max_source_bytes: 8 * 1024 * 1024,
                max_blocks: 1024,
                max_vertex_patches: 1024,
                max_work: SOURCE_WORK,
                buffer: BufferLimits {
                    max_frames: 1,
                    max_ghost_entries: 2,
                    max_extent_bytes: 16 * 1024,
                },
            },
        )
        .await
        .unwrap();
        let plan = prepare("MATCH (a)-[r:R]->(b) RETURN r, a.p, b.p");
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &plan, policy())
            .unwrap();
        for id in 0..EDGES {
            let output = cursor.next().await.unwrap().unwrap();
            let (row, charge) = output.into_parts();
            assert_eq!(
                row.values(),
                &[
                    GraphValue::Edge(EId(id as u128)),
                    GraphValue::Scalar(CanonicalScalar::Int(2 * id as i64)),
                    GraphValue::Scalar(CanonicalScalar::Int(2 * id as i64 + 1)),
                ],
            );
            drop(row);
            drop(charge);
        }
        assert!(cursor.next().await.is_none());
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn captured_only_probes_use_same_cut_stored_and_missing_nulls() {
    let ((), report) = run_async_under_lab(0xe6_52, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let mut stored = WriteBatch::new(R);
        stored.set_vertex_property(IDS[1], P, Some(CanonicalScalar::Null));
        db.write(&contexts.commit(), stored).await.unwrap();
        let mut missing = WriteBatch::new(R);
        missing.set_vertex_property(IDS[1], P, None);
        db.write(&contexts.commit(), missing).await.unwrap();
        let mut view = cold(&contexts, &vfs, memory()).await;
        for (predicate, before, after) in [
            (
                "EXISTS { MATCH (b) WHERE b.p IS NULL }",
                vec![],
                vec![0, 1, u128::MAX],
            ),
            (
                "EXISTS { MATCH (b) WHERE b.p>=2 }",
                vec![0, 1, 2, u128::MAX],
                vec![2],
            ),
            (
                "NOT EXISTS { MATCH (b) WHERE b.p<>NULL }",
                vec![0, 1, 2, 3, u128::MAX],
                vec![0, 1, 2, 3, u128::MAX],
            ),
        ] {
            let plan = prepare(&format!("MATCH (a)-[r:R]->(b) WHERE {predicate} RETURN r"));
            for cut in 1..=3 {
                let expected: Vec<_> = (if cut == 1 { &before } else { &after })
                    .iter()
                    .map(|id| vec![GraphValue::Edge(EId(*id))])
                    .collect();
                assert_eq!(
                    collect(&mut view, &cx, &plan, CommitSeq(cut)).await,
                    expected
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn anti_probe_refusal_cannot_publish_absence_or_revoke_prior_delivered_rows() {
    let ((), report) = run_async_under_lab(0xe6_53, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _db = seed(&contexts, &vfs).await;
        let pool = memory();
        let mut view = cold(&contexts, &vfs, pool.clone()).await;
        let plan = prepare(
            "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { \
            MATCH (b)-[s:R]->(c) WHERE s.p=999 } RETURN r",
        );
        let mut refused = view
            .stream_graph_edge_joins_governed(
                &cx,
                &plan,
                GqlQueryPolicy::new(1, 100, 10_000_000, 10_000_000),
            )
            .unwrap();
        assert!(matches!(
            refused.next().await,
            Some(Err(GqlQueryError::Rows(_)))
        ));
        assert_eq!(refused.row_stats().result_rows, 0);
        assert!(refused.next().await.is_none());
        drop(refused);
        let independent =
            prepare("MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (x) } RETURN r LIMIT 0");
        let before = pool.used();
        assert!(
            view.stream_graph_edge_joins_governed(&cx, &independent, policy())
                .is_err()
        );
        assert_eq!(
            pool.used(),
            before,
            "physical refusal precedes source construction"
        );
        let mut cursor = view
            .stream_graph_edge_joins_governed(
                &cx,
                &plan,
                GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000),
            )
            .unwrap();
        let held = cursor.next().await.unwrap().unwrap();
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Rows(_)))
        ));
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(view);
        assert!(pool.used() > 0);
        let (row, charge) = held.into_parts();
        assert_eq!(row.values(), &[GraphValue::Edge(EId(0))]);
        drop(row);
        drop(charge);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

async fn collect_vertices(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    plan: &PreparedGraphPattern<GraphValueRow>,
    cut: CommitSeq,
) -> Vec<Vec<GraphValue>> {
    let mut cursor = view
        .stream_graph_probes_governed_at(cx, plan, cut, policy())
        .unwrap();
    assert_eq!(cursor.snapshot_seq(), cut);
    let mut rows = Vec::new();
    while let Some(output) = cursor.next().await {
        let (row, charge) = output.unwrap().into_parts();
        rows.push(row.values().to_vec());
        drop(row);
        drop(charge);
    }
    assert_eq!(cursor.state(), fgdb_gql::stream::VertexScanState::Exhausted);
    assert!(cursor.next().await.is_none());
    rows
}

#[test]
fn cold_vertex_probes_match_historical_products_and_keep_isolated_root_records() {
    let ((), report) = run_async_under_lab(0xe6_57, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let pinned_pool = memory();
        let mut pinned = cold(&contexts, &vfs, pinned_pool.clone()).await;
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(IDS[0], P, Some(CanonicalScalar::Int(21)));
        change.delete_edge(EId(2));
        change.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Null)]);
        db.write(&contexts.commit(), change).await.unwrap();
        let pool = memory();
        let mut view = cold(&contexts, &vfs, pool.clone()).await;
        for cut in 0..=2 {
            for direction in 0..3 {
                for two_hops in [false, true] {
                    for anti in [false, true] {
                        for repeatable in [false, true] {
                            let child = atom("s", "c", direction);
                            let tail = if two_hops {
                                atom("t", "a", direction)
                            } else {
                                String::new()
                            };
                            let text = format!(
                                "MATCH (a) WHERE {}EXISTS {{ MATCH {}(a){child}{tail} WHERE s.p>=9 }} RETURN a,a.p AS value",
                                if anti { "NOT " } else { "" },
                                if repeatable {
                                    "REPEATABLE ELEMENTS "
                                } else {
                                    ""
                                },
                            );
                            let plan = prepare(&text);
                            let edges: Vec<_> = EDGES
                                .iter()
                                .filter(|(id, ..)| cut < 2 || *id != 2)
                                .collect();
                            let mut expected = Vec::new();
                            if cut != 0 {
                                for (a, vid) in IDS.into_iter().enumerate() {
                                    let witness = edges.iter().any(|&&(s, x, y, p)| {
                                        p >= 9
                                            && orient(x, y, direction).into_iter().any(
                                                |(from, c)| {
                                                    from == a
                                                        && (!two_hops
                                                            || edges.iter().any(
                                                                |&&(t, x, y, _)| {
                                                                    (repeatable || s != t)
                                                                        && orient(x, y, direction)
                                                                            .contains(&(c, a))
                                                                },
                                                            ))
                                                },
                                            )
                                    });
                                    if witness != anti {
                                        let value =
                                            if cut == 2 && a == 0 { 21 } else { a as i64 + 1 };
                                        expected.push(vec![
                                            GraphValue::Vertex(vid),
                                            GraphValue::Scalar(CanonicalScalar::Int(value)),
                                        ]);
                                    }
                                }
                                if cut == 2 && anti {
                                    expected.push(vec![
                                        GraphValue::Vertex(VId(1)),
                                        GraphValue::Scalar(CanonicalScalar::Null),
                                    ]);
                                }
                            }
                            expected.sort();
                            let resident = db
                                .execute_graph_pattern_governed_at(
                                    &cx,
                                    &plan,
                                    CommitSeq(cut),
                                    policy(),
                                )
                                .unwrap();
                            assert_eq!(
                                resident
                                    .value
                                    .iter()
                                    .map(|row| row.values().to_vec())
                                    .collect::<Vec<_>>(),
                                expected,
                                "resident {text} at {cut}"
                            );
                            assert_eq!(
                                collect_vertices(&mut view, &cx, &plan, CommitSeq(cut)).await,
                                expected,
                                "cold {text} at {cut}"
                            );
                        }
                    }
                }
            }
        }
        // Copy-only and identity paths must see the real isolated root. The
        // selected property is NULL; no edge endpoint can supply its record.
        for text in [
            "MATCH (a) WHERE EXISTS { MATCH (a) WHERE a.p IS NULL } RETURN a,a.p AS value",
            "MATCH (a) WHERE EXISTS { MATCH WALK (a)-[:R*0..0]->(b) WHERE b.p IS NULL } RETURN a,a.p AS value",
        ] {
            assert_eq!(
                collect_vertices(&mut view, &cx, &prepare(text), CommitSeq(2)).await,
                vec![vec![
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Scalar(CanonicalScalar::Null)
                ]]
            );
        }
        let plan = prepare(
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) WHERE b.p=3 } RETURN a,a.p AS value",
        );
        let old = vec![vec![
            GraphValue::Vertex(IDS[1]),
            GraphValue::Scalar(CanonicalScalar::Int(2)),
        ]];
        assert_eq!(
            collect_vertices(&mut pinned, &cx, &plan, CommitSeq(1)).await,
            old
        );
        db.compact(&contexts.commit()).await.unwrap();
        drop(view);
        let mut compacted = cold(&contexts, &vfs, pool.clone()).await;
        assert!(
            collect_vertices(&mut compacted, &cx, &plan, CommitSeq(2))
                .await
                .is_empty()
        );
        assert_eq!(
            collect_vertices(&mut pinned, &cx, &plan, CommitSeq(1)).await,
            old
        );
        drop(compacted);
        drop(pinned);
        assert_eq!(pool.used(), 0);
        assert_eq!(pinned_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_probe_admission_budgets_and_output_guards_keep_the_original_stream_contract() {
    let ((), report) = run_async_under_lab(0xe6_58, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _db = seed(&contexts, &vfs).await;
        let pool = memory();
        let mut view = cold(&contexts, &vfs, pool.clone()).await;
        let copied = prepare("MATCH (a) WHERE EXISTS { MATCH (a) } RETURN a");
        assert!(
            fgdb_gql::stream::AsyncVertexScanPlan::compile(copied.plan()).is_err(),
            "the ordinary source contract still refuses graph probes"
        );
        let mut cursor = view
            .stream_graph_probes_governed(&cx, &copied, policy())
            .unwrap();
        while let Some(output) = cursor.next().await {
            drop(output.unwrap());
        }
        assert_eq!(
            cursor.row_stats().snapshot_records,
            3,
            "copy probes never readmit their captured root"
        );
        assert_eq!(cursor.row_stats().result_rows, 3);
        drop(cursor);
        for text in [
            "MATCH (a) WHERE NOT EXISTS { MATCH (x) } RETURN a LIMIT 0",
            "MATCH (a) WHERE EXISTS { MATCH WALK (a)-[:R*]->(b) } RETURN a LIMIT 0",
        ] {
            let plan = prepare(text);
            let before = pool.used();
            assert!(
                view.stream_graph_probes_governed(&cx, &plan, policy())
                    .is_err(),
                "{text}"
            );
            assert_eq!(pool.used(), before, "refusal precedes source construction");
        }
        let plan =
            prepare("MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) WHERE b.p=999 } RETURN a");
        let mut refused = view
            .stream_graph_probes_governed(
                &cx,
                &plan,
                GqlQueryPolicy::new(1, 100, 10_000_000, 10_000_000),
            )
            .unwrap();
        assert!(matches!(
            refused.next().await,
            Some(Err(GqlQueryError::Rows(_)))
        ));
        assert_eq!(refused.row_stats().result_rows, 0);
        assert!(refused.next().await.is_none());
        drop(refused);
        let mut cursor = view
            .stream_graph_probes_governed(
                &cx,
                &plan,
                GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000),
            )
            .unwrap();
        let held = cursor.next().await.unwrap().unwrap();
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Rows(_)))
        ));
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.row_stats().result_rows, 1);
        drop(cursor);
        drop(view);
        assert!(
            pool.used() > 0,
            "delivered output retains its own byte guard"
        );
        let (row, charge) = held.into_parts();
        assert_eq!(row.values(), &[GraphValue::Vertex(IDS[0])]);
        drop(row);
        drop(charge);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
