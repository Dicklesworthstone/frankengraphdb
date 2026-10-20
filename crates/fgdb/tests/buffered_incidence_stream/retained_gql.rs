//! Cold GQL continuations against literal relation products and native reads.
use super::*;
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::QueryCx;

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn orient(row: &Row, direction: usize) -> Vec<(VId, VId)> {
    match direction {
        0 => vec![(row.1, row.2)],
        1 => vec![(row.2, row.1)],
        _ if row.1 == row.2 => vec![(row.1, row.2)],
        _ => vec![(row.1, row.2), (row.2, row.1)],
    }
}
fn payload(row: &Row) -> CanonicalScalar {
    row.4
        .iter()
        .find(|(key, _)| *key == P)
        .unwrap()
        .1
        .as_scalar()
        .expect("scalar property")
        .clone()
}
fn atom(edge: &str, end: &str, direction: usize, any: bool) -> String {
    let relation = if any { "" } else { ":R" };
    match direction {
        0 => format!("-[{edge}{relation}]->({end})"),
        1 => format!("<-[{edge}{relation}]-({end})"),
        _ => format!("-[{edge}{relation}]-({end})"),
    }
}
fn text(shape: usize, dirs: [usize; 2], any: bool, filtered: bool) -> String {
    let root = format!("MATCH (a){}", atom("r", "b", dirs[0], any));
    let (tail, end) = match shape {
        1 => (format!(", (a){}", atom("s", "c", dirs[1], any)), "c"),
        2 => (atom("s", "a", dirs[1], any), "a AS c"),
        _ => (atom("s", "c", dirs[1], any), "c"),
    };
    let predicate = if filtered { "WHERE s.p >= 9" } else { "" };
    format!("{root}{tail} {predicate} RETURN r,a,s,b,{end},s.p AS sp,a.p AS ap")
}
fn expected_pair(
    cut: u64,
    shape: usize,
    dirs: [usize; 2],
    any: bool,
    filtered: bool,
) -> Vec<Vec<GraphValue>> {
    let edges = expected(cut);
    let mut rows = Vec::new();
    for r in &edges {
        if !any && r.3 != R {
            continue;
        }
        for (a, b) in orient(r, dirs[0]) {
            for s in &edges {
                if r.0 == s.0 || (!any && s.3 != R) {
                    continue;
                }
                if filtered && !matches!(payload(s), CanonicalScalar::Int(n) if n >= 9) {
                    continue;
                }
                for (from, c) in orient(s, dirs[1]) {
                    if from != (if shape == 1 { a } else { b }) || (shape == 2 && c != a) {
                        continue;
                    }
                    let index = IDS.iter().position(|vid| *vid == a).unwrap();
                    let ap = if cut >= 2 && index == 0 {
                        21
                    } else {
                        index as i64 + 1
                    };
                    rows.push(vec![
                        GraphValue::Edge(r.0),
                        GraphValue::Vertex(a),
                        GraphValue::Edge(s.0),
                        GraphValue::Vertex(b),
                        GraphValue::Vertex(c),
                        GraphValue::Scalar(payload(s)),
                        GraphValue::Scalar(CanonicalScalar::Int(ap)),
                    ]);
                }
            }
        }
    }
    rows.sort();
    rows
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
        // This independent oracle copy is application-owned. Keep the source
        // result charge through every use of its original row, including drop.
        rows.push(row.values().to_vec());
        drop(row);
        drop(charge);
    }
    assert!(cursor.next().await.is_none());
    rows
}

#[test]
fn retained_gql_joins_match_literal_histories_and_pinned_compaction_views() {
    let ((), report) = run_async_under_lab(0xe6_43, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let pinned_memory = pool();
        let mut pinned = cold(&contexts, &vfs, pinned_memory.clone()).await;
        let mut second = WriteBatch::new(R);
        second.set_vertex_property(IDS[0], P, Some(CanonicalScalar::Int(21)));
        second.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(17)));
        second.delete_edge(EId(2));
        db.write(&contexts.commit(), second).await.unwrap();
        let mut third = WriteBatch::new(S);
        third.add_edge(EId(4), IDS[1], IDS[0], vec![(P, CanonicalScalar::Int(20))]);
        db.write(&contexts.commit(), third).await.unwrap();
        let memory = pool();
        let mut view = cold(&contexts, &vfs, memory.clone()).await;
        let mut cases = 0;
        for cut in 0..=3 {
            for shape in 0..3 {
                for left in 0..3 {
                    for right in 0..3 {
                        for any in [false, true] {
                            for filtered in [false, true] {
                                let text = text(shape, [left, right], any, filtered);
                                let plan = prepare(&text);
                                let wanted =
                                    expected_pair(cut, shape, [left, right], any, filtered);
                                let native = db
                                    .execute_graph_pattern_governed_at(
                                        &cx,
                                        &plan,
                                        CommitSeq(cut),
                                        policy(),
                                    )
                                    .unwrap();
                                assert_eq!(
                                    native
                                        .value
                                        .iter()
                                        .map(|row| row.values().to_vec())
                                        .collect::<Vec<_>>(),
                                    wanted,
                                    "resident {text} cut={cut}"
                                );
                                assert_eq!(
                                    collect(&mut view, &cx, &plan, CommitSeq(cut)).await,
                                    wanted,
                                    "cold {text} cut={cut}"
                                );
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 432);
        let plan = prepare(&text(0, [2, 2], true, true));
        let old = expected_pair(1, 0, [2, 2], true, true);
        let latest = expected_pair(3, 0, [2, 2], true, true);
        assert_eq!(collect(&mut pinned, &cx, &plan, CommitSeq(1)).await, old);
        db.compact(&contexts.commit()).await.unwrap();
        drop(db);
        let reopened =
            Database::open_with_vfs(&contexts.commit(), vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        let mut compacted = cold(&contexts, &vfs, pool()).await;
        assert_eq!(
            collect(&mut compacted, &cx, &plan, CommitSeq(3)).await,
            latest
        );
        assert_eq!(collect(&mut view, &cx, &plan, CommitSeq(3)).await, latest);
        assert_eq!(collect(&mut pinned, &cx, &plan, CommitSeq(1)).await, old);
        assert_eq!(reopened.frontier().unwrap(), CommitSeq(3));
        drop(pinned);
        drop(view);
        assert_eq!(pinned_memory.used(), 0);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn three_hop_backtracking_and_repeated_incidence_requests_preserve_occurrences() {
    let ((), report) = run_async_under_lab(0xe6_44, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let db = seed(&contexts, &vfs).await;
        let memory = pool();
        let mut view = cold(&contexts, &vfs, memory.clone()).await;
        let edges = expected(1);
        for star in [false, true] {
            for direction in [0, 2] {
                let first = atom("r", "b", direction, false);
                let second = atom("s", "c", direction, false);
                let third = atom("t", "d", direction, false);
                let head = if star {
                    format!("MATCH (a){first}, (a){second}, (a){third}")
                } else {
                    format!("MATCH (a){first}{second}{third}")
                };
                let text = format!("{head} RETURN r,a,s,t,b,c,d");
                let plan = prepare(&text);
                let mut wanted = Vec::new();
                for r in &edges {
                    for (a, b) in orient(r, direction) {
                        for s in &edges {
                            if s.0 == r.0 {
                                continue;
                            }
                            for (from, c) in orient(s, direction) {
                                if from != (if star { a } else { b }) {
                                    continue;
                                }
                                for t in &edges {
                                    if t.0 == r.0 || t.0 == s.0 {
                                        continue;
                                    }
                                    for (from, d) in orient(t, direction) {
                                        if from != (if star { a } else { c }) {
                                            continue;
                                        }
                                        wanted.push(vec![
                                            GraphValue::Edge(r.0),
                                            GraphValue::Vertex(a),
                                            GraphValue::Edge(s.0),
                                            GraphValue::Edge(t.0),
                                            GraphValue::Vertex(b),
                                            GraphValue::Vertex(c),
                                            GraphValue::Vertex(d),
                                        ]);
                                    }
                                }
                            }
                        }
                    }
                }
                wanted.sort();
                let native = db
                    .execute_graph_pattern_governed(&cx, &plan, policy())
                    .unwrap();
                assert_eq!(
                    native
                        .value
                        .iter()
                        .map(|row| row.values().to_vec())
                        .collect::<Vec<_>>(),
                    wanted,
                    "{text}"
                );
                assert_eq!(
                    collect(&mut view, &cx, &plan, CommitSeq(1)).await,
                    wanted,
                    "{text}"
                );
                let paged = prepare(&format!("{text} SKIP 1 LIMIT 3"));
                assert_eq!(
                    collect(&mut view, &cx, &paged, CommitSeq(1)).await,
                    wanted.into_iter().skip(1).take(3).collect::<Vec<_>>()
                );
            }
        }
        drop(view);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retained_gql_late_result_refusal_fuses_without_revoking_a_delivered_row() {
    let ((), report) = run_async_under_lab(0xe6_45, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _db = seed(&contexts, &vfs).await;
        let memory = pool();
        let mut view = cold(&contexts, &vfs, memory.clone()).await;
        let plan = prepare(&text(0, [0, 0], false, false));
        let mut refused = view
            .stream_graph_edge_joins_governed(
                &cx,
                &plan,
                GqlQueryPolicy::new(100_000, 100_000, 0, 10_000_000),
            )
            .unwrap();
        assert!(matches!(
            refused.next().await,
            Some(Err(GqlQueryError::Evaluator(_)))
        ));
        assert!(refused.next().await.is_none());
        drop(refused);
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
        assert!(cursor.next().await.is_none());
        cursor.close();
        drop(cursor);
        drop(view);
        assert!(memory.used() > 0);
        let (row, charge) = held.into_parts();
        let wanted = expected_pair(1, 0, [0, 0], false, false);
        assert_eq!(row.values(), wanted[0].as_slice());
        drop(row);
        drop(charge);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
