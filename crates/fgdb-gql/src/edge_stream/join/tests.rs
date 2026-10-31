use super::*;
use crate::algebra::{GraphValue, PreparedGraphPattern};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(7);
const P: PropertyKeyId = PropertyKeyId(9);
type EdgeFixtures = BTreeMap<EId, (VId, VId, Vec<(PropertyKeyId, CanonicalPropertyValue)>)>;
#[derive(Clone)]
struct Fixture {
    edges: EdgeFixtures,
    vertices: BTreeSet<VId>,
    outgoing: BTreeMap<VId, BTreeSet<EId>>,
    incoming: BTreeMap<VId, BTreeSet<EId>>,
    after: Option<EId>,
    roots: Arc<AtomicUsize>,
    seeks: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    unavailable: bool,
    repeat: bool,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Fixture {
    fn from_edges(edges: impl IntoIterator<Item = (u128, u128, u128)>) -> Self {
        let mut f = Self {
            edges: BTreeMap::new(),
            vertices: BTreeSet::new(),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            after: None,
            roots: Arc::new(AtomicUsize::new(0)),
            seeks: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
            unavailable: false,
            repeat: false,
        };
        for (id, from, to) in edges {
            f.vertices.extend([VId(from), VId(to)]);
            f.outgoing.entry(VId(from)).or_default().insert(EId(id));
            f.incoming.entry(VId(to)).or_default().insert(EId(id));
            f.edges.insert(
                EId(id),
                (
                    VId(from),
                    VId(to),
                    vec![(P, CanonicalScalar::Int((id % 5) as i64 - 2).into())],
                ),
            );
        }
        f
    }
    fn small(mask: usize) -> Self {
        Self::from_edges(
            [
                (0, 0, 0),
                (1, 0, 1),
                (2, 0, 1),
                (3, 1, 2),
                (4, 2, 0),
                (u128::MAX, 2, 2),
            ]
            .into_iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, e)| e),
        )
    }
}
impl EdgeScanSource for Fixture {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(13)
    }
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        self.roots.fetch_add(1, Ordering::SeqCst);
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let id = self
            .edges
            .range((self.after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(&id, _)| id);
        if id.is_some() {
            self.after = id;
        }
        Ok(id)
    }
    fn edge<'a, C>(
        &'a self,
        id: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.edges.get(&id).map(|(a, b, p)| EdgeScanRow {
            source: *a,
            target: *b,
            relation: R,
            properties: p,
        }))
    }
    fn vertex<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.vertices.contains(&id).then_some(VertexScanRow {
            labels: &[],
            properties: &[],
        }))
    }
    fn next_incident_edge<C>(
        &self,
        at: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.seeks.fetch_add(1, Ordering::SeqCst);
        if self.unavailable {
            return Err(EdgeExpansionSourceError::Unavailable);
        }
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        if self.repeat && after.is_some() {
            return Ok(after);
        }
        let seek = |face: &BTreeMap<VId, BTreeSet<EId>>| {
            face.get(&at).and_then(|s| {
                s.range((after.map_or(Unbounded, Excluded), Unbounded))
                    .next()
                    .copied()
            })
        };
        Ok(match direction {
            GlaDirection::Forward => seek(&self.outgoing),
            GlaDirection::Reverse => seek(&self.incoming),
            GlaDirection::Undirected => match (seek(&self.outgoing), seek(&self.incoming)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        })
    }
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000)
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|r| r.values().to_vec()).collect()
}
fn atom(edge: &str, end: &str, d: GlaDirection) -> String {
    match d {
        GlaDirection::Forward => format!("-[{edge}:R]->({end})"),
        GlaDirection::Reverse => format!("<-[{edge}:R]-({end})"),
        GlaDirection::Undirected => format!("-[{edge}:R]-({end})"),
    }
}
fn statement(
    shape: usize,
    d: [GlaDirection; 2],
    filter: bool,
    skip: usize,
    limit: usize,
) -> String {
    let head = format!("MATCH (a){}", atom("r", "b", d[0]));
    let next = match shape {
        1 => format!(", (a){}", atom("s", "c", d[1])),
        2 => atom("s", "a", d[1]),
        _ => atom("s", "c", d[1]),
    };
    let end = if shape == 2 { "a AS c" } else { "c" };
    format!(
        "{head}{next} {} RETURN r, a, s, b, {end}, r.p AS rp, s.p AS sp SKIP {skip} LIMIT {limit}",
        if filter {
            "WHERE r.p >= 0 AND s.p < 2"
        } else {
            ""
        }
    )
}
// Independent finite relation join: expand every orientation of both edges,
// join their endpoints, then sort and paginate complete rows (no cursor state).
fn oracle(
    f: &Fixture,
    shape: usize,
    d: [GlaDirection; 2],
    filter: bool,
    skip: usize,
    limit: usize,
) -> Vec<Vec<GraphValue>> {
    let orientations = |a: VId, b: VId, d| match d {
        GlaDirection::Forward => vec![(a, b)],
        GlaDirection::Reverse => vec![(b, a)],
        GlaDirection::Undirected if a == b => vec![(a, b)],
        _ => vec![(a, b), (b, a)],
    };
    let mut rows = Vec::new();
    for (&r, (x, y, rp)) in &f.edges {
        for (a, b) in orientations(*x, *y, d[0]) {
            for (&s, (x, y, sp)) in &f.edges {
                if r == s {
                    continue;
                }
                for (from, c) in orientations(*x, *y, d[1]) {
                    if from != (if shape == 1 { a } else { b }) || (shape == 2 && c != a) {
                        continue;
                    }
                    if filter
                        && !matches!((rp[0].1.as_scalar().unwrap(), sp[0].1.as_scalar().unwrap()), (CanonicalScalar::Int(x), CanonicalScalar::Int(y)) if *x >= 0 && *y < 2)
                    {
                        continue;
                    }
                    rows.push(vec![
                        GraphValue::Edge(r),
                        GraphValue::Vertex(a),
                        GraphValue::Edge(s),
                        GraphValue::Vertex(b),
                        GraphValue::Vertex(c),
                        GraphValue::Scalar(rp[0].1.as_scalar().unwrap().clone()),
                        GraphValue::Scalar(sp[0].1.as_scalar().unwrap().clone()),
                    ]);
                }
            }
        }
    }
    rows.sort();
    rows.into_iter().skip(skip).take(limit).collect()
}

#[test]
fn different_edges_keeps_parallel_return_paths_and_repeatable_mode_keeps_backtracking() {
    let query = "MATCH (a)-[r:R]-(b)-[s:R]-(a) RETURN r, a, s";
    for (edges, different, repeatable) in [
        (vec![(1, 1, 2)], 0, 2),
        (vec![(1, 1, 2), (2, 1, 2)], 4, 8),
        (vec![(1, 1, 1)], 0, 1),
        (vec![(1, 1, 1), (2, 1, 1)], 2, 4),
    ] {
        for (text, expected) in [
            (query.to_owned(), different),
            (
                query.replacen("MATCH ", "MATCH DIFFERENT EDGES ", 1),
                different,
            ),
            (
                query.replacen("MATCH ", "MATCH REPEATABLE ELEMENTS ", 1),
                repeatable,
            ),
        ] {
            let prepared = prepare(&text);
            let rows = EdgeScanCursor::new(
                Fixture::from_edges(edges.iter().copied()),
                EdgeScanPlan::compile(prepared.plan()).unwrap(),
                policy(),
                || Ok::<_, ()>(()),
            )
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
            assert_eq!(rows.len(), expected, "{text}: {edges:?}");
            if !text.contains("REPEATABLE ELEMENTS") {
                assert!(rows.iter().all(|row| row.values()[0] != row.values()[2]));
            }
        }
    }
}

#[test]
fn chains_branches_and_cycle_closures_match_cartesian_and_eager_oracles_in_every_direction() {
    let dirs = [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ];
    for mask in [0, 10, 31, 63] {
        for shape in 0..3 {
            for left in dirs {
                for right in dirs {
                    for filter in [false, true] {
                        for (skip, limit) in [(0, 1000), (1, 3), (0, 0)] {
                            let text = statement(shape, [left, right], filter, skip, limit);
                            for distinct in [false, true] {
                                let text = if distinct {
                                    text.replace("RETURN ", "RETURN DISTINCT ")
                                } else {
                                    text.clone()
                                };
                                let q = prepare(&text);
                                let f = Fixture::small(mask);
                                let want = oracle(&f, shape, [left, right], filter, skip, limit);
                                let eager = q
                                    .plan()
                                    .execute_governed_with_element_properties(
                                        f.edges.len() as u64,
                                        f.vertices.iter().copied(),
                                        f.edges.iter().map(|(&id, (a, b, _))| (id, *a, R, *b)),
                                        |_, _| Ok::<_, ()>(true),
                                        |_, _| Ok(None),
                                        |id, _| Ok(Some(&f.edges[&id].2[0].1)),
                                        policy(),
                                        || Ok::<_, ()>(()),
                                    )
                                    .unwrap();
                                assert_eq!(plain(&eager.value), want, "eager: {text}");
                                let mut cursor = EdgeScanCursor::new(
                                    f,
                                    EdgeScanPlan::compile(q.plan()).unwrap(),
                                    policy(),
                                    || Ok::<_, ()>(()),
                                );
                                let mut rows = Vec::new();
                                for page in [1, 2, 1024] {
                                    for _ in 0..page {
                                        let Some(row) = cursor.next() else {
                                            break;
                                        };
                                        rows.push(row.unwrap());
                                    }
                                }
                                assert_eq!(plain(&rows), want, "stream: {text}");
                                assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn shared_capture_projection_keeps_path_steps_edge_properties_and_repeated_bindings() {
    let text = "MATCH p = (a)-[r:R]->(b)-[s:R]->(c) RETURN r, a, s, p, path_length(p) AS n, r.p AS rp, s.p AS sp";
    let q = prepare(text);
    let f = Fixture::small(63);
    let eager = q
        .plan()
        .execute_governed_with_element_properties(
            6,
            f.vertices.iter().copied(),
            f.edges.iter().map(|(&id, (a, b, _))| (id, *a, R, *b)),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            |id, _| Ok(Some(&f.edges[&id].2[0].1)),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let rows = EdgeScanCursor::new(
        f,
        EdgeScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(rows, eager.value);
}

#[test]
fn every_checkpoint_refuses_once_drops_frames_and_retries_the_complete_query() {
    let q = prepare(&statement(0, [GlaDirection::Undirected; 2], false, 0, 5));
    let mut calls = 0;
    let mut good = EdgeScanCursor::new(
        Fixture::small(63),
        EdgeScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || {
            calls += 1;
            Ok::<_, usize>(())
        },
    );
    let expected = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    drop(good);
    for stop in 1..=calls {
        let mut at = 0;
        let f = Fixture::small(63);
        let dropped = f.drops.clone();
        let mut cursor = EdgeScanCursor::new(
            f,
            EdgeScanPlan::compile(q.plan()).unwrap(),
            policy(),
            || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        let mut prefix = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => prefix.push(row),
                Some(Err(GqlQueryError::Interrupted(n))) => {
                    assert_eq!(n, stop);
                    break;
                }
                other => panic!("refusal became another result: {other:?}"),
            }
        }
        assert_eq!(prefix, expected[..prefix.len()]);
        assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.traversal.is_none());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(cursor.next().is_none());
        drop(cursor);
        assert_eq!(at, stop);
        let retry = EdgeScanCursor::new(
            Fixture::small(63),
            EdgeScanPlan::compile(q.plan()).unwrap(),
            policy(),
            || Ok::<_, usize>(()),
        )
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        assert_eq!(retry, expected);
    }
}

#[test]
fn exact_limits_are_cumulative_and_each_smaller_allowance_refuses_instead_of_eof() {
    let q = prepare(&statement(1, [GlaDirection::Forward; 2], false, 0, 4));
    let run = |p| {
        EdgeScanCursor::new(
            Fixture::small(63),
            EdgeScanPlan::compile(q.plan()).unwrap(),
            p,
            || Ok::<_, ()>(()),
        )
    };
    let mut full = run(policy());
    let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let r = full.row_stats();
    let e = full.evaluator_stats();
    let exact = GqlQueryPolicy::new(
        r.snapshot_records,
        r.result_rows,
        e.work_units,
        e.scratch_entries,
    );
    let mut retry = run(exact);
    assert_eq!(
        retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        expected
    );
    assert_eq!((retry.row_stats(), retry.evaluator_stats()), (r, e));
    for p in [
        GqlQueryPolicy::new(r.snapshot_records - 1, 100, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(100, r.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(100, 100, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(100, 100, u64::MAX, e.scratch_entries - 1),
    ] {
        let mut c = run(p);
        assert!(c.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(c.state(), EdgeScanState::Failed);
        assert!(c.next().is_none());
    }
}

#[test]
fn maximum_depth_and_exponential_join_output_keep_only_one_prefix_and_obey_early_close() {
    for hops in [40, MAX_PATTERN_EDGES] {
        let mut text = "MATCH (v0)".to_owned();
        for i in 0..hops {
            text.push_str(&format!("-[e{i}:R]->(v{})", i + 1));
        }
        text.push_str(" RETURN e0, v0");
        for i in 1..hops {
            text.push_str(&format!(", e{i}"));
        }
        text.push_str(" LIMIT 1");
        let q = prepare(&text);
        let f = Fixture::from_edges(
            (0..hops as u128).flat_map(|i| [(i * 2, i, i + 1), (i * 2 + 1, i, i + 1)]),
        );
        let roots = f.roots.clone();
        let seeks = f.seeks.clone();
        let mut c = EdgeScanCursor::new(
            f,
            EdgeScanPlan::compile(q.plan()).unwrap(),
            GqlQueryPolicy::new(hops as u64, 1, 100_000, 10_000),
            || Ok::<_, ()>(()),
        );
        assert_eq!(roots.load(Ordering::SeqCst), 0);
        assert_eq!(c.next().unwrap().unwrap().values().len(), hops + 1);
        assert_eq!(roots.load(Ordering::SeqCst), 1);
        assert_eq!(seeks.load(Ordering::SeqCst), hops - 1);
        assert_eq!(c.row_stats().snapshot_records, hops as u64);
        assert!(c.traversal.is_none());
        c.close();
        let stats = c.evaluator_stats();
        assert!(c.next().is_none());
        assert_eq!(c.evaluator_stats(), stats);
    }
}

#[test]
fn directed_join_identity_prefix_needs_no_synthetic_source_vertex_column() {
    for mask in [0, 10, 31, 63] {
        for shape in 0..3 {
            for root in [GlaDirection::Forward, GlaDirection::Reverse] {
                for next in [
                    GlaDirection::Forward,
                    GlaDirection::Reverse,
                    GlaDirection::Undirected,
                ] {
                    for distinct in [false, true] {
                        for (skip, limit) in [(0, 1000), (1, 3), (0, 0)] {
                            let text = statement(shape, [root, next], false, skip, limit)
                                .replace("r, a, s", "r, s");
                            let text = if distinct {
                                text.replace("RETURN ", "RETURN DISTINCT ")
                            } else {
                                text
                            };
                            let query = prepare(&text);
                            let fixture = Fixture::small(mask);
                            // The finite relation oracle enumerates orientations
                            // independently, then removes only the unused root
                            // vertex from complete rows before the native check.
                            let expected: Vec<_> =
                                oracle(&fixture, shape, [root, next], false, skip, limit)
                                    .into_iter()
                                    .map(|mut row| {
                                        row.remove(1);
                                        row
                                    })
                                    .collect();
                            let eager = query
                                .plan()
                                .execute_governed_with_element_properties(
                                    fixture.edges.len() as u64,
                                    fixture.vertices.iter().copied(),
                                    fixture.edges.iter().map(|(&id, (a, b, _))| (id, *a, R, *b)),
                                    |_, _| Ok::<_, ()>(true),
                                    |_, _| Ok(None),
                                    |id, _| Ok(Some(&fixture.edges[&id].2[0].1)),
                                    policy(),
                                    || Ok::<_, ()>(()),
                                )
                                .unwrap();
                            assert_eq!(plain(&eager.value), expected, "{text}, mask {mask}");
                            let roots = Arc::clone(&fixture.roots);
                            let mut cursor = EdgeScanCursor::new(
                                fixture,
                                EdgeScanPlan::compile(query.plan()).unwrap(),
                                policy(),
                                || Ok::<_, ()>(()),
                            );
                            assert_eq!(roots.load(Ordering::SeqCst), 0);
                            let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                            assert_eq!(plain(&rows), expected, "{text}, mask {mask}");
                            assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                            assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                        }
                    }
                }
            }
        }
    }
    for text in [
        "MATCH (a)-[r:R]-(b)-[s:R]->(c) RETURN r, s LIMIT 0",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r, c, s LIMIT 0",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN s, r LIMIT 0",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r LIMIT 0",
    ] {
        assert!(
            EdgeScanPlan::compile(prepare(text).plan()).is_err(),
            "{text}"
        );
    }
}

#[test]
fn private_join_keys_match_eager_and_independent_property_occurrences_in_every_direction() {
    let directions = [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ];
    for shape in 0..3 {
        for root in directions {
            for next in directions {
                for (skip, limit) in [(0, 1000), (1, 3), (0, 0)] {
                    let full = statement(shape, [root, next], false, skip, limit);
                    let head = full.split_once(" RETURN ").unwrap().0;
                    let keys = if root == GlaDirection::Undirected {
                        "r, a, s"
                    } else {
                        "r, s"
                    };
                    let text = format!(
                        "{head} RETURN ALL s.p AS sp, r.p AS rp \
                         ORDER BY {keys}, sp DESC NULLS FIRST SKIP {skip} LIMIT {limit}"
                    );
                    let query = prepare(&text);
                    let mut fixture = Fixture::small(63);
                    for (&id, (_, _, properties)) in &mut fixture.edges {
                        properties[0].1 = if id == EId(u128::MAX) {
                            CanonicalScalar::Null.into()
                        } else {
                            CanonicalScalar::Int(7).into()
                        };
                    }
                    let expected: Vec<_> =
                        oracle(&fixture, shape, [root, next], false, skip, limit)
                            .into_iter()
                            .map(|row| vec![row[6].clone(), row[5].clone()])
                            .collect();
                    let eager = query
                        .plan()
                        .execute_governed_with_element_properties(
                            fixture.edges.len() as u64,
                            fixture.vertices.iter().copied(),
                            fixture.edges.iter().map(|(&id, (a, b, _))| (id, *a, R, *b)),
                            |_, _| Ok::<_, ()>(true),
                            |_, _| Ok(None),
                            |id, _| Ok(Some(&fixture.edges[&id].2[0].1)),
                            policy(),
                            || Ok::<_, ()>(()),
                        )
                        .unwrap();
                    assert_eq!(plain(&eager.value), expected, "{text}");
                    let roots = fixture.roots.clone();
                    let mut cursor = EdgeScanCursor::new(
                        fixture,
                        EdgeScanPlan::compile(query.plan()).unwrap(),
                        policy(),
                        || Ok::<_, ()>(()),
                    );
                    assert_eq!(roots.load(Ordering::SeqCst), 0);
                    let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                    assert_eq!(plain(&rows), expected, "{text}");
                    assert!(rows.iter().all(|row| row.values().len() == 2));
                    assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                }
            }
        }
    }
}

#[test]
fn private_join_keys_admit_repeatable_occurrences_but_refuse_incomplete_traversal_order() {
    for text in [
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN s.p AS value ORDER BY r",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN s.p AS value ORDER BY s, r",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN s.p AS value ORDER BY r, s DESC",
        "MATCH (a)-[r:R]-(b)-[s:R]->(c) RETURN s.p AS value ORDER BY r, s",
        "MATCH (a)-[r:R]-(b)-[s:R]->(c) RETURN s.p AS value ORDER BY r, s, a",
        "MATCH (a)-[r:R]-(b)-[s:R]->(c) RETURN s.p AS value ORDER BY r, a DESC, s",
    ] {
        assert!(
            EdgeScanPlan::compile(prepare(text).plan()).is_err(),
            "{text}"
        );
    }
    let text = "MATCH REPEATABLE ELEMENTS (a)-[r:R]-(b)-[s:R]-(a) \
                RETURN r.p AS first, s.p AS second ORDER BY r, a, s";
    let query = prepare(text);
    let fixture = Fixture::from_edges([(u128::MAX, 0, u128::MAX)]);
    let eager = query
        .plan()
        .execute_governed_with_element_properties(
            1,
            fixture.vertices.iter().copied(),
            fixture.edges.iter().map(|(&id, (a, b, _))| (id, *a, R, *b)),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            |id, _| Ok(Some(&fixture.edges[&id].2[0].1)),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let actual = EdgeScanCursor::new(
        fixture,
        EdgeScanPlan::compile(query.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(actual, eager.value);
    assert_eq!(actual.len(), 2);
    assert_eq!(
        actual[0], actual[1],
        "orientation occurrences remain duplicates"
    );
    assert_eq!(actual[0].values().len(), 2);
}

#[test]
fn unsupported_sources_bad_order_and_invalid_plan_prefixes_never_silently_change_semantics() {
    let text = statement(0, [GlaDirection::Forward; 2], false, 0, 5);
    for invalid in [
        text.replace("r, a, s", "r, a, b AS missing"),
        text.replace("r, a, s", "s, a, r"),
        text.replace("RETURN r", "RETURN a AS first, r"),
        text.replace(" SKIP 0", " ORDER BY sp SKIP 0"),
        "MATCH (a)-[r:R]->(b)-[:R*1..2]->(c) RETURN r, a, b, c".to_owned(),
    ] {
        let q = prepare(&invalid);
        assert!(EdgeScanPlan::compile(q.plan()).is_err(), "{invalid}");
    }
    let q = prepare(&text);
    let mut f = Fixture::small(63);
    f.unavailable = true;
    let mut c = EdgeScanCursor::new(
        f,
        EdgeScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        c.next(),
        Some(Err(GqlQueryError::Source(
            EdgeScanError::ExpansionUnavailable
        )))
    ));
    let mut f = Fixture::small(63);
    f.repeat = true;
    let mut c = EdgeScanCursor::new(
        f,
        EdgeScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    // The repeated identity is reported where it lands, never skipped. Under
    // the DIFFERENT EDGES default the self-loop pair that used to come first
    // is not a row, so it lands on the first call (fgdb-vexfr).
    assert!(matches!(
        c.next(),
        Some(Err(GqlQueryError::Source(
            EdgeScanError::NonIncreasingIdentity
        )))
    ));
    let q = prepare(&text.replace("LIMIT 5", "LIMIT 0"));
    let f = Fixture::small(63);
    let roots = f.roots.clone();
    let mut c = EdgeScanCursor::new(
        f,
        EdgeScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(c.next().is_none());
    assert_eq!(roots.load(Ordering::SeqCst), 0);
}
