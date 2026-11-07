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
    fail_edge: Option<EId>,
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
            fail_edge: None,
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
        if self.fail_edge == Some(id) {
            return Err(EdgeScanSourceError::Source("edge history unreadable"));
        }
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

mod collections {
    use super::*;
    use crate::algebra::{
        GraphColumn, GraphMatchClause, GraphMatchMode, GraphPatternBuilder, GraphValueOrder,
    };

    type ProjectedResult<C> =
        Result<GraphValueRow, GqlQueryError<crate::scan_stream::ScanError<&'static str>, C>>;

    fn projected_next<C, F: FnMut() -> Result<(), C>>(
        cursor: &mut EdgeScanCursor<Fixture, F>,
        pull: &crate::GraphSetRowPull,
    ) -> Option<ProjectedResult<C>> {
        use crate::scan_stream::ScanError;
        cursor.next_projected(
            pull,
            |error| error.map_source(ScanError::Edge),
            |row, column, error| {
                GqlQueryError::Source(ScanError::Projection { row, column, error })
            },
        )
    }

    fn projected(text: &str) -> crate::GraphSetRowPull {
        crate::PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .edge_row_pull()
            .unwrap()
    }

    #[test]
    fn projected_join_rows_keep_the_complete_identity_prefix_and_literal_properties() {
        let directions = [
            ("-", "->", GlaDirection::Forward),
            ("<-", "-", GlaDirection::Reverse),
            ("-", "-", GlaDirection::Undirected),
        ];
        for (left, right, root) in directions {
            for (next_left, next_right, next) in directions {
                let pull = projected(&format!(
                    "MATCH REPEATABLE ELEMENTS (a){left}[r:R]{right}(b)\
                     {next_left}[s:R]{next_right}(c) RETURN r,a,s,b,c,r.p+s.p AS total,\
                     [(c)-[t:R]->(d) | t.p] AS weights"
                ));
                let plan = EdgeScanPlan::compile(pull.pattern().plan()).unwrap();
                for mask in 0..64 {
                    let fixture = Fixture::small(mask);
                    let mut expected = Vec::new();
                    for (&r, (a, b, _)) in &fixture.edges {
                        for (a, b) in orientations(*a, *b, root) {
                            for (&s, (start, c, _)) in &fixture.edges {
                                for (start, c) in orientations(*start, *c, next) {
                                    if start != b {
                                        continue;
                                    }
                                    let weights = fixture
                                        .edges
                                        .iter()
                                        .filter(|(_, (from, _, _))| *from == c)
                                        .map(|(id, _)| {
                                            GraphValue::Scalar(CanonicalScalar::Int(
                                                (id.0 % 5) as i64 - 2,
                                            ))
                                        })
                                        .collect();
                                    expected.push(GraphValueRow::from_owned_values(vec![
                                        GraphValue::Edge(r),
                                        GraphValue::Vertex(a),
                                        GraphValue::Edge(s),
                                        GraphValue::Vertex(b),
                                        GraphValue::Vertex(c),
                                        GraphValue::Scalar(CanonicalScalar::Int(
                                            (r.0 % 5) as i64 + (s.0 % 5) as i64 - 4,
                                        )),
                                        list(weights),
                                    ]));
                                }
                            }
                        }
                    }
                    expected.sort();
                    let mut cursor =
                        EdgeScanCursor::new(fixture, plan.clone(), policy(), || Ok::<_, ()>(()));
                    let mut actual = Vec::new();
                    while let Some(row) = projected_next(&mut cursor, &pull) {
                        actual.push(row.unwrap());
                    }
                    assert_eq!(actual, expected, "{root:?}/{next:?}; mask={mask}");
                    assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
                    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                }
            }
        }
    }

    #[test]
    fn projected_collection_properties_preserve_literal_multigraph_order_in_every_direction() {
        for (outer_left, outer_right, outer) in [
            ("-", "->", GlaDirection::Forward),
            ("<-", "-", GlaDirection::Reverse),
            ("-", "-", GlaDirection::Undirected),
        ] {
            for (inner_left, inner_right, inner) in [
                ("-", "->", GlaDirection::Forward),
                ("<-", "-", GlaDirection::Reverse),
                ("-", "-", GlaDirection::Undirected),
            ] {
                let pull = projected(&format!(
                    "MATCH (a){outer_left}[r:R]{outer_right}(b) RETURN r,a,b,\
                     [(b){inner_left}[s:R]{inner_right}(c) | s.p] AS weights"
                ));
                let plan = EdgeScanPlan::compile(pull.pattern().plan()).unwrap();
                for mask in 0..64 {
                    let fixture = Fixture::small(mask);
                    let expected: Vec<_> = expected(&fixture, outer, inner)
                        .into_iter()
                        .map(|row| {
                            let weights = row.values()[4]
                                .as_list()
                                .unwrap()
                                .iter()
                                .map(|value| {
                                    let GraphValue::Edge(id) = value else {
                                        panic!("edge witness")
                                    };
                                    let value =
                                        fixture.edges[id].2[0].1.as_scalar().unwrap().clone();
                                    GraphValue::Scalar(value)
                                })
                                .collect();
                            GraphValueRow::from_owned_values(vec![
                                row.values()[0].clone(),
                                row.values()[1].clone(),
                                row.values()[2].clone(),
                                GraphValue::List(weights),
                            ])
                        })
                        .collect();
                    let roots = fixture.roots.clone();
                    let mut cursor =
                        EdgeScanCursor::new(fixture, plan.clone(), policy(), || Ok::<_, ()>(()));
                    assert_eq!(roots.load(Ordering::SeqCst), 0);
                    assert_eq!(cursor.row_stats().snapshot_records, 0);
                    let mut actual = Vec::new();
                    while let Some(row) = projected_next(&mut cursor, &pull) {
                        actual.push(row.unwrap());
                    }
                    assert_eq!(actual, expected, "{outer:?}/{inner:?}; mask={mask}");
                    assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
                    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                    assert!(projected_next(&mut cursor, &pull).is_none());
                }
            }
        }
    }

    #[test]
    fn projected_collections_share_every_checkpoint_and_exact_cumulative_allowance() {
        let pull = projected(
            "MATCH (a)-[r:R]->(b) RETURN r,a,b,\
             [(b)-[s:R]->(c)-[t:R]->(d) | s.p+t.p] AS weights",
        );
        let plan = EdgeScanPlan::compile(pull.pattern().plan()).unwrap();
        let mut checkpoints = 0;
        let mut cursor = EdgeScanCursor::new(Fixture::small(63), plan.clone(), policy(), || {
            checkpoints += 1;
            Ok::<_, usize>(())
        });
        let mut expected = Vec::new();
        while let Some(row) = projected_next(&mut cursor, &pull) {
            expected.push(row.unwrap());
        }
        let rows = cursor.row_stats();
        let evaluator = cursor.evaluator_stats();
        drop(cursor);
        assert!(checkpoints > 0);
        for stop in 1..=checkpoints {
            let fixture = Fixture::small(63);
            let drops = fixture.drops.clone();
            let mut at = 0;
            let mut cursor = EdgeScanCursor::new(fixture, plan.clone(), policy(), || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            });
            let mut prefix = Vec::new();
            loop {
                match projected_next(&mut cursor, &pull) {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Interrupted(reported))) => {
                        assert_eq!(reported, stop);
                        break;
                    }
                    other => panic!("checkpoint {stop} became {other:?}"),
                }
            }
            assert_eq!(prefix, expected[..prefix.len()]);
            assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(cursor.source.is_none());
            assert!(cursor.traversal.is_none());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(projected_next(&mut cursor, &pull).is_none());
            drop(cursor);
            assert_eq!(at, stop);
        }
        let exact = GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            evaluator.work_units,
            evaluator.scratch_entries,
        );
        let mut retry =
            EdgeScanCursor::new(Fixture::small(63), plan.clone(), exact, || Ok::<_, ()>(()));
        let mut actual = Vec::new();
        while let Some(row) = projected_next(&mut retry, &pull) {
            actual.push(row.unwrap());
        }
        assert_eq!(actual, expected);
        assert_eq!(
            (retry.row_stats(), retry.evaluator_stats()),
            (rows, evaluator)
        );
        for allowance in [
            GqlQueryPolicy::new(rows.snapshot_records - 1, 100, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1_000, rows.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1_000, 100, evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1_000, 100, u64::MAX, evaluator.scratch_entries - 1),
        ] {
            let mut cursor =
                EdgeScanCursor::new(Fixture::small(63), plan.clone(), allowance, || {
                    Ok::<_, ()>(())
                });
            let mut delivered = 0;
            loop {
                match projected_next(&mut cursor, &pull) {
                    Some(Ok(row)) => {
                        assert_eq!(row, expected[delivered]);
                        delivered += 1;
                    }
                    Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))) => break,
                    other => panic!("short allowance became {other:?}"),
                }
            }
            assert_eq!(cursor.row_stats().result_rows, delivered as u64);
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(projected_next(&mut cursor, &pull).is_none());
        }
    }

    #[test]
    fn projected_collection_failure_keeps_the_original_row_and_column_without_counting_it() {
        let pull = projected(
            "MATCH (a)-[r:R]->(b) RETURN r,a,b,10/(r.p+1) AS q,\
                              [(b)-[s:R]->(c) | s.p] AS weights",
        );
        let fixture = Fixture::small(63);
        let drops = fixture.drops.clone();
        let mut cursor = EdgeScanCursor::new(
            fixture,
            EdgeScanPlan::compile(pull.pattern().plan()).unwrap(),
            policy(),
            || Ok::<_, ()>(()),
        );
        assert_eq!(
            projected_next(&mut cursor, &pull)
                .unwrap()
                .unwrap()
                .values()[0],
            GraphValue::Edge(EId(0))
        );
        assert!(matches!(
            projected_next(&mut cursor, &pull),
            Some(Err(GqlQueryError::Source(
                crate::scan_stream::ScanError::Projection {
                    row: 1,
                    column: 3,
                    ..
                }
            )))
        ));
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(projected_next(&mut cursor, &pull).is_none());
    }

    #[test]
    fn projected_edge_admission_checks_keys_and_every_hidden_effect_before_opening() {
        let prepare = |text: &str| {
            crate::PreparedGraphSetText::prepare(text, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap()
        };
        let base = "MATCH (a)-[r:R]->(b) RETURN r,a,b,[(b)-[s:R]->(c) | s.p] AS weights";
        assert!(prepare(base).edge_row_pull().is_some());
        assert!(prepare(&format!("{base} LIMIT 0")).edge_row_pull().is_some());
        for invalid in [
            base.replace("RETURN r,a,b", "RETURN a,r,b"),
            base.replace("RETURN r,a,b", "RETURN DISTINCT r,a,b"),
            format!("{base} ORDER BY weights LIMIT 0"),
            base.replace(
                "RETURN r,a,b",
                "RETURN r,CASE WHEN false THEN rand() ELSE 0 END AS x,a,b",
            ),
            base.replace(
                "RETURN r,a,b",
                "RETURN r,CASE WHEN false THEN properties(a) ELSE 0 END AS x,a,b",
            ),
            base.replace("[r:R]->(b)", "[r:R]-(b)")
                .replace("RETURN r,a,b", "RETURN r,b,a"),
            base.replace("(b) RETURN", "(b)-[t:R]->(d) RETURN"),
        ] {
            assert!(prepare(&invalid).edge_row_pull().is_none(), "{invalid}");
        }
    }

    fn definition(
        outer_direction: GlaDirection,
        inner_direction: GlaDirection,
        offset: u64,
        count: Option<u64>,
    ) -> PreparedGraphPattern<GraphValueRow> {
        let mut outer = GraphPatternBuilder::new();
        outer.vertex("a").unwrap().vertex("b").unwrap();
        outer.edge("a", R, outer_direction, "b").unwrap();
        outer.capture_edge("r", 0).unwrap();
        let mut inner = GraphPatternBuilder::new();
        inner.vertex("b").unwrap().vertex("c").unwrap();
        inner.edge("b", R, inner_direction, "c").unwrap();
        inner.capture_edge("s", 0).unwrap();
        inner.capture_path("p").unwrap();
        outer
            .prepare_values_with_clauses(
                &[
                    GraphMatchClause::collect(&inner, Some("c")),
                    GraphMatchClause::collect(&inner, Some("s")),
                    GraphMatchClause::collect(&inner, Some("p")),
                    GraphMatchClause::collect_tuple(&inner, vec!["b", "s", "c", "p"]),
                    GraphMatchClause::collect(&inner, None),
                ],
                &[
                    GraphColumn::path("r", "r", GraphPathFunction::Edge),
                    GraphColumn::vertex("a", "a"),
                    GraphColumn::vertex("b", "b"),
                    GraphColumn::collection("vertices", 0),
                    GraphColumn::collection("edges", 1),
                    GraphColumn::collection("paths", 2),
                    GraphColumn::collection("tuples", 3),
                    GraphColumn::collection("nulls", 4),
                ],
                offset,
                count,
            )
            .unwrap()
    }

    fn multi_hop_definition(
        directions: [GlaDirection; 2],
        mode: GraphMatchMode,
        branch: bool,
    ) -> PreparedGraphPattern<GraphValueRow> {
        let mut outer = GraphPatternBuilder::new();
        outer.vertex("a").unwrap().vertex("b").unwrap();
        outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
        outer.capture_edge("r", 0).unwrap();
        let mut inner = GraphPatternBuilder::new();
        inner
            .vertex("b")
            .unwrap()
            .vertex("c")
            .unwrap()
            .vertex("d")
            .unwrap();
        inner.edge("b", R, directions[0], "c").unwrap();
        inner
            .edge(if branch { "b" } else { "c" }, R, directions[1], "d")
            .unwrap();
        inner.capture_edge("s", 0).unwrap();
        inner.capture_edge("t", 1).unwrap();
        inner.match_mode(mode);
        outer
            .prepare_values_with_clauses(
                &[GraphMatchClause::collect_tuple(
                    &inner,
                    vec!["b", "s", "c", "t", "d", "c"],
                )],
                &[
                    GraphColumn::path("r", "r", GraphPathFunction::Edge),
                    GraphColumn::vertex("a", "a"),
                    GraphColumn::vertex("b", "b"),
                    GraphColumn::collection("tuples", 0),
                ],
                0,
                None,
            )
            .unwrap()
    }

    fn list(values: Vec<GraphValue>) -> GraphValue {
        GraphValue::List(values.into_boxed_slice())
    }

    fn orientations(a: VId, b: VId, direction: GlaDirection) -> Vec<(VId, VId)> {
        match direction {
            GlaDirection::Forward => vec![(a, b)],
            GlaDirection::Reverse => vec![(b, a)],
            GlaDirection::Undirected if a == b => vec![(a, b)],
            GlaDirection::Undirected => vec![(a, b), (b, a)],
        }
    }

    // A complete finite edge-product oracle, independent of cursor traversal.
    // The inner list keeps EId order, repetitions, orientation and tuple
    // association; only complete outer rows are canonically sorted.
    fn expected(
        fixture: &Fixture,
        outer_direction: GlaDirection,
        inner_direction: GlaDirection,
    ) -> Vec<GraphValueRow> {
        let mut rows = Vec::new();
        for (&r, (x, y, _)) in &fixture.edges {
            for (a, b) in orientations(*x, *y, outer_direction) {
                let mut vertices = Vec::new();
                let mut edges = Vec::new();
                let mut paths = Vec::new();
                let mut tuples = Vec::new();
                let mut nulls = Vec::new();
                for (&s, (u, v, _)) in &fixture.edges {
                    for (start, c) in orientations(*u, *v, inner_direction) {
                        if start != b {
                            continue;
                        }
                        let path =
                            GraphValue::Path(GraphPath::new(b, vec![(s, c)].into_boxed_slice()));
                        vertices.push(GraphValue::Vertex(c));
                        edges.push(GraphValue::Edge(s));
                        paths.push(path.clone());
                        tuples.push(list(vec![
                            GraphValue::Vertex(b),
                            GraphValue::Edge(s),
                            GraphValue::Vertex(c),
                            path,
                        ]));
                        nulls.push(GraphValue::Scalar(CanonicalScalar::Null));
                    }
                }
                rows.push(GraphValueRow::from_owned_values(vec![
                    GraphValue::Edge(r),
                    GraphValue::Vertex(a),
                    GraphValue::Vertex(b),
                    list(vertices),
                    list(edges),
                    list(paths),
                    list(tuples),
                    list(nulls),
                ]));
            }
        }
        rows.sort();
        rows
    }

    fn eager(query: &PreparedGraphPattern<GraphValueRow>, fixture: &Fixture) -> Vec<GraphValueRow> {
        query
            .plan()
            .execute_governed_with_element_properties(
                (fixture.vertices.len() + fixture.edges.len()) as u64,
                fixture.vertices.iter().copied(),
                fixture
                    .edges
                    .iter()
                    .rev()
                    .map(|(&eid, (a, b, _))| (eid, *a, R, *b)),
                |_, predicates| Ok::<_, ()>(predicates.iter().all(|p| p.matches(&[], &[]))),
                |_, _| Ok(None),
                |eid, key| {
                    Ok(fixture.edges[&eid]
                        .2
                        .iter()
                        .find(|(property, _)| *property == key)
                        .map(|(_, value)| value))
                },
                policy(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value
    }

    #[test]
    fn collected_single_hops_match_eager_and_finite_multigraph_oracles() {
        let directions = [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ];
        for outer in directions {
            for inner in directions {
                let query = definition(outer, inner, 0, None);
                let plan = EdgeScanPlan::compile(query.plan()).unwrap();
                for mask in 0..64 {
                    let fixture = Fixture::small(mask);
                    let expected = expected(&fixture, outer, inner);
                    assert_eq!(eager(&query, &fixture), expected);
                    let actual =
                        EdgeScanCursor::new(fixture, plan.clone(), policy(), || Ok::<_, ()>(()))
                            .collect::<Result<Vec<_>, _>>()
                            .unwrap();
                    assert_eq!(actual, expected, "{outer:?}/{inner:?}; mask={mask}");
                }
            }
        }
    }

    #[test]
    fn collected_fixed_hop_chains_and_branches_match_complete_edge_products() {
        let directions = [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ];
        for branch in [false, true] {
            for first in directions {
                for second in directions {
                    for mode in [
                        GraphMatchMode::RepeatableElements,
                        GraphMatchMode::DifferentEdges,
                    ] {
                        let query = multi_hop_definition([first, second], mode, branch);
                        let plan = EdgeScanPlan::compile(query.plan()).unwrap();
                        for mask in 0..64 {
                            let fixture = Fixture::small(mask);
                            let mut expected = Vec::new();
                            // Enumerate the complete three-edge product without
                            // using the native cursor or eager traversal helpers.
                            for (&r, (a, b, _)) in &fixture.edges {
                                let mut tuples = Vec::new();
                                for (&s, (from, to, _)) in &fixture.edges {
                                    for (start, c) in orientations(*from, *to, first) {
                                        if start != *b {
                                            continue;
                                        }
                                        for (&t, (from, to, _)) in &fixture.edges {
                                            if mode == GraphMatchMode::DifferentEdges && s == t {
                                                continue;
                                            }
                                            for (start, d) in orientations(*from, *to, second) {
                                                if start != if branch { *b } else { c } {
                                                    continue;
                                                }
                                                // A collection has its own edge domain:
                                                // reusing the outer r remains legal.
                                                tuples.push(list(vec![
                                                    GraphValue::Vertex(*b),
                                                    GraphValue::Edge(s),
                                                    GraphValue::Vertex(c),
                                                    GraphValue::Edge(t),
                                                    GraphValue::Vertex(d),
                                                    GraphValue::Vertex(c),
                                                ]));
                                            }
                                        }
                                    }
                                }
                                expected.push(GraphValueRow::from_owned_values(vec![
                                    GraphValue::Edge(r),
                                    GraphValue::Vertex(*a),
                                    GraphValue::Vertex(*b),
                                    list(tuples),
                                ]));
                            }
                            expected.sort();
                            assert_eq!(
                                eager(&query, &fixture),
                                expected,
                                "eager {first:?}/{second:?}; {mode:?}; branch={branch}; mask={mask}",
                            );
                            let actual =
                                EdgeScanCursor::new(fixture, plan.clone(), policy(), || {
                                    Ok::<_, ()>(())
                                })
                                .collect::<Result<Vec<_>, _>>()
                                .unwrap();
                            assert_eq!(
                                actual, expected,
                                "native {first:?}/{second:?}; {mode:?}; branch={branch}; mask={mask}",
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn fixed_hop_collection_admission_preserves_scope_and_capture_refusals() {
        use crate::algebra::{BindingSlot, GraphWalkSearch, ValueProjection};

        let query = multi_hop_definition(
            [GlaDirection::Forward; 2],
            GraphMatchMode::DifferentEdges,
            false,
        );
        let original = query.plan().operators();
        let start = original
            .iter()
            .position(|op| matches!(op, GlaOperator::Collect { .. }))
            .unwrap();
        let (_, end) = probe::Collection::compile(original, start, 2).unwrap();
        let expansions: Vec<_> = (start + 1..end)
            .filter(|&at| matches!(original[at], GlaOperator::Expand { .. }))
            .collect();
        let captures: Vec<_> = (start + 1..end)
            .filter(|&at| matches!(original[at], GlaOperator::CapturePath { .. }))
            .collect();
        assert_eq!(expansions.len(), 2);
        assert_eq!(captures.len(), 2);
        for corruption in 0..6 {
            let mut ops = original.to_vec();
            match corruption {
                0 => ops[start + 1] = GlaOperator::ScanVertices,
                1 => {
                    for &at in &expansions {
                        ops[at] = GlaOperator::BindVertex {
                            source: BindingSlot(0),
                        };
                    }
                }
                2 => {
                    let GlaOperator::Expand {
                        source,
                        relation,
                        direction,
                    } = ops[expansions[0]].clone()
                    else {
                        unreachable!("the fixture includes two fixed expansions")
                    };
                    ops[expansions[0]] = GlaOperator::VarLengthExpand {
                        source,
                        relation,
                        direction,
                        bounds: crate::GraphWalkBounds::new(1, 2).unwrap(),
                        search: GraphWalkSearch::All,
                        edge_predicates: Vec::new(),
                    };
                }
                3 => {
                    ops[captures[0]] = GlaOperator::Probe {
                        group: 1,
                        end: end as u32,
                        anti: false,
                    };
                }
                4 => {
                    let GlaOperator::CapturePath { segments, .. } = ops[captures[1]].clone() else {
                        unreachable!("the fixture captures both relationships")
                    };
                    let GlaOperator::CapturePath {
                        segments: first, ..
                    } = &mut ops[captures[0]]
                    else {
                        unreachable!("the fixture captures both relationships")
                    };
                    first.extend(segments);
                }
                5 => {
                    let GlaOperator::CollectEnd { value, tuple, .. } = &mut ops[end] else {
                        unreachable!("the fixture closes its collection")
                    };
                    *value = Some(ValueProjection::Path {
                        capture: 0, // The outer r must not become a local edge or NULL.
                        function: GraphPathFunction::Edge,
                    });
                    *tuple = None;
                }
                _ => unreachable!(),
            }
            assert!(
                probe::Collection::compile(&ops, start, 2).is_err(),
                "unsupported or malformed scope {corruption}",
            );
        }
        // Multi-hop output still needs an owned byte reservation before any
        // awaitable physical lane can admit it.
        assert!(AsyncEdgeJoinPlan::compile(query.plan()).is_err());
        assert!(AsyncEdgeJoinPlan::compile_sort_input(query.plan()).is_err());
    }

    #[test]
    fn later_collection_hop_failure_discards_already_collected_witnesses() {
        let query = multi_hop_definition(
            [GlaDirection::Forward; 2],
            GraphMatchMode::DifferentEdges,
            false,
        );
        let plan = EdgeScanPlan::compile(query.plan()).unwrap();
        let mut fixture = Fixture::from_edges([(0, 0, 1), (1, 1, 2), (2, 2, 3), (3, 2, 4)]);
        assert_eq!(
            eager(&query, &fixture)[0].values()[3]
                .as_list()
                .unwrap()
                .len(),
            2,
        );
        // Edge 2 completes the first private witness. Edge 3 fails only
        // when advancing the second hop, before the outer row can publish.
        fixture.fail_edge = Some(EId(3));
        let drops = fixture.drops.clone();
        let mut cursor = EdgeScanCursor::new(fixture, plan, policy(), || Ok::<_, ()>(()));
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(EdgeScanError::Source(
                "edge history unreadable"
            ))))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.source.is_none());
        assert!(cursor.traversal.is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(cursor.next().is_none());
    }

    #[test]
    fn collection_scope_predicates_keep_captured_edges_and_outer_values_distinct() {
        let query = crate::PreparedGraphSetText::prepare(
            "MATCH (a)-[r:R]->(b) RETURN r,a,b,\
             [(b)-[s:R]->(c)-[t:R]->(d) \
              WHERE s.p >= 0 AND t.p < 2 AND d <> a | d] AS matches",
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let pattern = query.single_pattern_input().unwrap();
        let fixture = Fixture::small(63);
        let expected = eager(pattern, &fixture);
        let actual = EdgeScanCursor::new(
            fixture,
            EdgeScanPlan::compile(pattern.plan()).unwrap(),
            policy(),
            || Ok::<_, ()>(()),
        )
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        assert_eq!(actual, expected);
        assert!(actual.iter().any(|row| {
            row.values()
                .iter()
                .any(|value| value.as_list().is_some_and(|list| !list.is_empty()))
        }));
    }

    #[test]
    fn collected_sort_input_keeps_all_rows_before_the_blocking_page() {
        let query = definition(GlaDirection::Forward, GlaDirection::Undirected, 2, Some(1))
            .with_order_by(&[GraphValueOrder::descending(3)])
            .unwrap();
        assert!(EdgeScanPlan::compile(query.plan()).is_err());
        let (plan, tail) = EdgeScanPlan::compile_sort_input(query.plan()).unwrap();
        assert_eq!(tail.offset(), 2);
        assert_eq!(tail.count(), Some(1));
        assert_eq!(tail.order(), &[GraphValueOrder::descending(3)]);
        let fixture = Fixture::small(63);
        let expected = expected(&fixture, GlaDirection::Forward, GlaDirection::Undirected);
        let actual = EdgeScanCursor::new(fixture, plan, policy(), || Ok::<_, ()>(()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(actual, expected, "the sort consumer owns the final page");

        let query = definition(GlaDirection::Forward, GlaDirection::Forward, 0, Some(0));
        let (plan, _) = EdgeScanPlan::compile_sort_input(query.plan()).unwrap();
        let mut fixture = Fixture::small(63);
        fixture.fail_edge = Some(EId(1));
        let mut cursor = EdgeScanCursor::new(fixture, plan, policy(), || Ok::<_, ()>(()));
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(EdgeScanError::Source(
                "edge history unreadable"
            ))))
        ));
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
    }

    #[test]
    fn every_collection_checkpoint_cancels_without_a_partial_row_and_retry_is_exact() {
        let query = definition(GlaDirection::Forward, GlaDirection::Forward, 0, Some(2));
        let plan = EdgeScanPlan::compile(query.plan()).unwrap();
        let mut checkpoints = 0;
        let mut cursor = EdgeScanCursor::new(Fixture::small(63), plan.clone(), policy(), || {
            checkpoints += 1;
            Ok::<_, usize>(())
        });
        let expected = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let rows = cursor.row_stats();
        let evaluator = cursor.evaluator_stats();
        drop(cursor);
        for stop in 1..=checkpoints {
            let fixture = Fixture::small(63);
            let drops = fixture.drops.clone();
            let mut at = 0;
            let mut cursor = EdgeScanCursor::new(fixture, plan.clone(), policy(), || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            });
            let mut prefix = Vec::new();
            loop {
                match cursor.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Interrupted(reported))) => {
                        assert_eq!(reported, stop);
                        break;
                    }
                    other => panic!("checkpoint {stop} became {other:?}"),
                }
            }
            assert_eq!(prefix, expected[..prefix.len()]);
            assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(cursor.traversal.is_none());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(cursor.next().is_none());
            drop(cursor);
            assert_eq!(at, stop);
        }
        let exact = GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            evaluator.work_units,
            evaluator.scratch_entries,
        );
        let mut retry =
            EdgeScanCursor::new(Fixture::small(63), plan.clone(), exact, || Ok::<_, ()>(()));
        assert_eq!(
            retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            expected
        );
        assert_eq!(
            (retry.row_stats(), retry.evaluator_stats()),
            (rows, evaluator)
        );
        for allowance in [
            GqlQueryPolicy::new(rows.snapshot_records - 1, 100, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1_000, rows.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1_000, 100, evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1_000, 100, u64::MAX, evaluator.scratch_entries - 1),
        ] {
            let mut cursor =
                EdgeScanCursor::new(Fixture::small(63), plan.clone(), allowance, || {
                    Ok::<_, ()>(())
                });
            assert!(matches!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>(),
                Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))
            ));
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(cursor.next().is_none());
        }
    }

    #[test]
    fn incomplete_collection_reads_refuse_instead_of_publishing_a_prefix() {
        let query = definition(GlaDirection::Forward, GlaDirection::Forward, 0, None);
        let plan = EdgeScanPlan::compile(query.plan()).unwrap();
        for failure in 0..3 {
            let mut fixture = Fixture::small(63);
            fixture.unavailable = failure == 0;
            fixture.repeat = failure == 1;
            fixture.fail_edge = (failure == 2).then_some(EId(1));
            let dropped = fixture.drops.clone();
            let mut cursor =
                EdgeScanCursor::new(fixture, plan.clone(), policy(), || Ok::<_, ()>(()));
            assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(_)))));
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(cursor.next().is_none());
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
        }
        // Async byte guards do not yet reserve retained collection payloads.
        // Preparation must continue to refuse, even at LIMIT zero.
        let query = definition(GlaDirection::Forward, GlaDirection::Forward, 0, Some(0));
        assert!(AsyncEdgeJoinPlan::compile(query.plan()).is_err());
        assert!(AsyncEdgeJoinPlan::compile_sort_input(query.plan()).is_err());
        let fixture = Fixture::small(63);
        let roots = fixture.roots.clone();
        let mut cursor = EdgeScanCursor::new(
            fixture,
            EdgeScanPlan::compile(query.plan()).unwrap(),
            policy(),
            || Ok::<_, ()>(()),
        );
        assert!(cursor.next().is_none());
        assert_eq!(roots.load(Ordering::SeqCst), 0);
    }
}
