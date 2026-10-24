//! Public vertex-source tests for completed-group output, not a second store.
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::stream::aggregate::{VertexAggregateCursor, VertexAggregatePlan};
use fgdb_gql::stream::{
    VertexScanEvent, VertexScanRow, VertexScanSource, VertexScanSourceError, VertexScanState,
};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
    GraphSymbolResolver, PreparedGraphAggregate, PreparedGraphAggregateText, RelationBind,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, CommitSeq, VId};
use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

const G: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const LABELS: [LabelId; 1] = [LabelId(1)];
#[derive(Clone)]
struct Row {
    id: VId,
    props: Vec<(PropertyKeyId, CanonicalPropertyValue)>,
}
struct Source {
    rows: Vec<Row>,
    at: usize,
    fail: Option<usize>,
    polls: Rc<Cell<usize>>,
    dropped: Rc<Cell<bool>>,
}
impl VertexScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(17)
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        self.polls.set(self.polls.get() + 1);
        if self.fail == Some(self.at) {
            return Err(VertexScanSourceError::Source("late source"));
        }
        let id = self.rows.get(self.at).map(|row| row.id);
        self.at += usize::from(id.is_some());
        Ok(id)
    }
    fn vertex<C>(
        &self,
        id: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'_>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        Ok(self
            .rows
            .iter()
            .find(|row| row.id == id)
            .map(|row| VertexScanRow {
                labels: &LABELS,
                properties: &row.props,
            }))
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}
fn source(rows: Vec<Row>) -> Source {
    Source {
        rows,
        at: 0,
        fail: None,
        polls: Rc::new(Cell::new(0)),
        dropped: Rc::new(Cell::new(false)),
    }
}
fn row(id: u128, group: Option<i64>, value: Option<i64>) -> Row {
    let mut props = Vec::new();
    if let Some(value) = group {
        props.push((G, CanonicalScalar::Int(value).into()));
    }
    if let Some(value) = value {
        props.push((P, CanonicalScalar::Int(value).into()));
    }
    Row { id: VId(id), props }
}
fn fixture(mask: u32) -> Vec<Row> {
    [
        (Some(0), Some(7)),
        (Some(0), None),
        (Some(1), Some(2)),
        (Some(2), Some(2)),
        (None, Some(-3)),
        (Some(3), Some(7)),
    ]
    .into_iter()
    .enumerate()
    .filter(|(at, _)| mask & (1_u32 << *at) != 0)
    .map(|(at, (g, p))| row(if at == 5 { u128::MAX } else { at as u128 }, g, p))
    .collect()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn query(text: &str) -> PreparedGraphAggregate {
    let mut names = RelationBind::new()
        .with_label("L", LABELS[0])
        .with_property("g", G)
        .with_property("p", P);
    PreparedGraphAggregateText::prepare(text, |kind, name| names.resolve_symbol(kind, name))
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn run(
    q: &PreparedGraphAggregate,
    s: Source,
    policy: GqlQueryPolicy,
) -> VertexAggregateCursor<Source, impl FnMut() -> Result<(), ()>> {
    VertexAggregateCursor::new(s, VertexAggregatePlan::compile(q).unwrap(), policy, || {
        Ok(())
    })
}
fn integer(row: &Row, key: PropertyKeyId) -> Option<i64> {
    row.props.iter().find_map(|(k, v)| match v {
        CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)) if *k == key => Some(*n),
        _ => None,
    })
}
fn eager(q: &PreparedGraphAggregate, rows: &[Row]) -> Vec<GraphAggregateRow> {
    q.execute_governed(
        rows.len() as u64,
        rows.iter().map(|r| r.id),
        [],
        |id, predicates| {
            Ok::<_, ()>(
                rows.iter()
                    .find(|r| r.id == id)
                    .is_some_and(|r| predicates.iter().all(|p| p.matches(&LABELS, &r.props))),
            )
        },
        |id, key| {
            Ok(rows
                .iter()
                .find(|r| r.id == id)
                .and_then(|r| r.props.iter().find(|(k, _)| *k == key).map(|(_, v)| v)))
        },
        wide(),
        || Ok::<_, ()>(()),
    )
    .unwrap()
    .value
}
// Independent group arithmetic, hidden SUM ordering, representative selection,
// and final page. This never invokes a production sort/comparator/aggregate.
fn counts(
    rows: &[Row],
    descending: bool,
    nulls_first: bool,
    distinct: bool,
    offset: usize,
    count: usize,
) -> Vec<u64> {
    let mut groups: BTreeMap<Option<i64>, (u64, Option<i128>)> = BTreeMap::new();
    for row in rows {
        let group = groups.entry(integer(row, G)).or_default();
        group.0 += 1;
        if let Some(value) = integer(row, P) {
            group.1 = Some(group.1.unwrap_or(0) + i128::from(value));
        }
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by(|(ka, (_, a)), (kb, (_, b))| {
        let cmp = match (a, b) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => {
                if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (Some(_), None) => {
                if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (Some(a), Some(b)) => {
                if descending {
                    b.cmp(a)
                } else {
                    a.cmp(b)
                }
            }
        };
        cmp.then_with(|| ka.cmp(kb))
    });
    let mut seen = BTreeSet::new();
    groups
        .into_iter()
        .map(|(_, (n, _))| n)
        .filter(|n| !distinct || seen.insert(*n))
        .skip(offset)
        .take(count)
        .collect()
}

#[test]
fn hidden_rank_distinct_and_pages_match_independent_complete_groups() {
    for mask in 0..64 {
        for descending in [false, true] {
            for nulls_first in [false, true] {
                for distinct in [false, true] {
                    for (skip, take) in [(0, 0), (0, 1), (1, 3), (99, 1)] {
                        let q = query(&format!(
                            "MATCH (v:L) RETURN {}COUNT(*) AS total GROUP BY v.g HAVING total>=1 ORDER BY SUM(v.p) {} NULLS {} SKIP {skip} LIMIT {take}",
                            if distinct { "DISTINCT " } else { "" },
                            if descending { "DESC" } else { "ASC" },
                            if nulls_first { "FIRST" } else { "LAST" }
                        ));
                        let rows = fixture(mask);
                        let expected = counts(&rows, descending, nulls_first, distinct, skip, take);
                        let before = q.canonical_bytes();
                        let batch = eager(&q, &rows);
                        let mut cursor = run(&q, source(rows), wide());
                        let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                        assert_eq!(
                            actual
                                .iter()
                                .map(|r| r.values()[0].as_count().unwrap())
                                .collect::<Vec<_>>(),
                            expected
                        );
                        assert_eq!(actual, batch);
                        assert_eq!(q.canonical_bytes(), before);
                        assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
                        assert_eq!(cursor.state(), VertexScanState::Exhausted);
                        assert!(cursor.next().is_none());
                    }
                }
            }
        }
    }
}

#[test]
fn qualified_off_page_errors_and_late_source_errors_precede_all_output() {
    let rows = vec![
        row(0, Some(0), Some(1)),
        row(1, Some(0), Some(1)),
        row(2, Some(1), Some(1)),
    ];
    for limit in [0, 1] {
        for suffix in ["", " ORDER BY COUNT(*) DESC"] {
            let q = query(&format!(
                "MATCH (v:L) RETURN DISTINCT 1/(COUNT(*)-1) AS bad GROUP BY v.g{suffix} LIMIT {limit}"
            ));
            let mut cursor = run(&q, source(rows.clone()), wide());
            assert!(matches!(
                cursor.next(),
                Some(Err(GqlQueryError::Source(
                    GraphAggregateError::OutputExpression { .. }
                )))
            ));
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
            // HAVING rejection occurs before output-expression evaluation.
            let q = query(&format!(
                "MATCH (v:L) RETURN 1/(COUNT(*)-1) AS good GROUP BY v.g HAVING COUNT(*)>1{suffix} LIMIT {limit}"
            ));
            assert_eq!(
                run(&q, source(rows.clone()), wide())
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
                    .len(),
                limit as usize
            );
            let q = query(&format!(
                "MATCH (v:L) RETURN v.g,COUNT(*) AS total GROUP BY v.g{suffix} LIMIT {limit}"
            ));
            let mut s = source(rows.clone());
            s.fail = Some(rows.len());
            let dropped = s.dropped.clone();
            let mut cursor = run(&q, s, wide());
            assert!(cursor.next().unwrap().is_err());
            assert!(dropped.get());
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert!(cursor.next().is_none());
        }
    }
    let q = query("MATCH (v:L) RETURN MIN(v.p) AS m GROUP BY v.g HAVING m>0 OR TRUE LIMIT 0");
    let mut bad = rows;
    bad[2].props[1].1 = CanonicalScalar::Bool(true).into();
    assert!(matches!(
        run(&q, source(bad), wide()).next(),
        Some(Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerHaving { .. }
        )))
    ));
}

#[test]
fn only_selected_output_consumes_row_quota_and_empty_globals_keep_their_semantics() {
    let rows: Vec<_> = (0..1024).map(|i| row(i, Some(i as i64), Some(1))).collect();
    for text in [
        "MATCH (v:L) RETURN v AS id,COUNT(*) AS total GROUP BY v ORDER BY id DESC LIMIT 1",
        "MATCH (v:L) RETURN DISTINCT COUNT(*) AS total GROUP BY v.g LIMIT 1",
        "MATCH (v:L) RETURN v.g AS g,COUNT(*) AS total GROUP BY v.g HAVING g=1023 LIMIT 1",
    ] {
        let q = query(text);
        let mut cursor = run(
            &q,
            source(rows.clone()),
            GqlQueryPolicy::new(1024, 1, u64::MAX, u64::MAX),
        );
        assert!(cursor.next().unwrap().is_ok());
        assert!(cursor.next().is_none());
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert_eq!(cursor.row_stats().snapshot_records, 1024);
    }
    for text in [
        "MATCH (v:L) RETURN COUNT(*) AS total HAVING total>0",
        "MATCH (v:L) RETURN COUNT(*) AS total LIMIT 0",
        "MATCH (v:L) RETURN DISTINCT COUNT(*) AS total GROUP BY v.g ORDER BY total",
    ] {
        let q = query(text);
        let mut cursor = run(
            &q,
            source(vec![]),
            GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX),
        );
        assert!(cursor.next().is_none());
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
    }
    let q = query(
        "MATCH (v:L) RETURN COUNT(*) AS total,AVG(v.p) AS average HAVING total=0 ORDER BY total LIMIT 1",
    );
    let result = run(&q, source(vec![]), wide()).next().unwrap().unwrap();
    assert_eq!(result.values()[0].as_count(), Some(0));
    assert!(result.values()[1].is_null());
}

#[test]
fn transformed_output_keeps_exact_domains_repeated_keys_and_all_input_columns() {
    for text in [
        "MATCH (v:L) RETURN v.g AS first,v.g AS again,SUM(v.p)+COUNT(*) AS adjusted,AVG(v.p) AS average GROUP BY v.g ORDER BY AVG(v.p) DESC NULLS FIRST LIMIT 3",
        "MATCH (v:L) RETURN DISTINCT AVG(DISTINCT v.p) AS mean GROUP BY v.g ORDER BY SUM(v.p) DESC",
        "MATCH (v:L) RETURN SUM(v.p+1) AS adjusted GROUP BY v.g HAVING adjusted>0 ORDER BY adjusted DESC LIMIT 2",
        "MATCH (v:L) RETURN COUNT(*) AS total GROUP BY v ORDER BY v DESC LIMIT 1",
    ] {
        let q = query(text);
        let rows = fixture(63);
        let expected = eager(&q, &rows);
        assert_eq!(
            run(&q, source(rows), wide())
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected,
            "{text}"
        );
    }
    // Result processing may not conceal a bad aggregate input at LIMIT 0.
    let q = query("MATCH (v:L) RETURN SUM(v.p) AS total GROUP BY v.g ORDER BY total LIMIT 0");
    let mut bad = fixture(63);
    bad.last_mut().unwrap().props[1].1 = CanonicalScalar::Bool(true).into();
    assert!(matches!(
        run(&q, source(bad), wide()).next(),
        Some(Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerSum { .. }
        )))
    ));
}

#[test]
fn every_checkpoint_and_one_less_quota_fuses_and_releases_retained_state() {
    let q = query(
        "MATCH (v:L) RETURN DISTINCT COUNT(*) AS total GROUP BY v.g HAVING total>0 ORDER BY SUM(v.p) DESC SKIP 0 LIMIT 2",
    );
    let calls = Rc::new(Cell::new(0));
    let observed = calls.clone();
    let mut full = VertexAggregateCursor::new(
        source(fixture(63)),
        VertexAggregatePlan::compile(&q).unwrap(),
        wide(),
        move || {
            observed.set(observed.get() + 1);
            Ok::<_, usize>(())
        },
    );
    let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let r = full.row_stats();
    let e = full.evaluator_stats();
    assert_eq!(expected.len(), 2);
    let exact = GqlQueryPolicy::new(
        r.snapshot_records,
        r.result_rows,
        e.work_units,
        e.scratch_entries,
    );
    assert_eq!(
        run(&q, source(fixture(63)), exact)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        expected
    );
    for stop in 1..=calls.get() {
        let s = source(fixture(63));
        let dropped = s.dropped.clone();
        let mut at = 0;
        let mut cursor = VertexAggregateCursor::new(
            s,
            VertexAggregatePlan::compile(&q).unwrap(),
            exact,
            move || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        let mut delivered = Vec::new();
        loop {
            match cursor.next().expect("selected checkpoint must be reached") {
                Ok(row) => delivered.push(row),
                Err(GqlQueryError::Interrupted(actual)) => {
                    assert_eq!(actual, stop);
                    break;
                }
                Err(error) => panic!("unexpected refusal: {error:?}"),
            }
        }
        assert!(expected.starts_with(&delivered));
        assert!(dropped.get());
        assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
    for policy in [
        GqlQueryPolicy::new(r.snapshot_records - 1, 2, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 2, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 2, u64::MAX, e.scratch_entries - 1),
    ] {
        let mut cursor = run(&q, source(fixture(63)), policy);
        assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn close_never_drains_and_collections_use_the_same_output_stage() {
    let q = query("MATCH (v:L) RETURN v.g,COUNT(*) AS total GROUP BY v.g ORDER BY total LIMIT 3");
    let s = source(fixture(63));
    let polls = s.polls.clone();
    let dropped = s.dropped.clone();
    let mut cursor = run(&q, s, wide());
    cursor.close();
    cursor.close();
    assert_eq!(polls.get(), 0);
    assert!(dropped.get());
    assert!(cursor.next().is_none());
    let mut cursor = run(&q, source(fixture(63)), wide());
    assert!(cursor.next().unwrap().is_ok());
    let stats = (cursor.row_stats(), cursor.evaluator_stats());
    cursor.close();
    assert_eq!(cursor.state(), VertexScanState::Closed);
    assert!(cursor.next().is_none());
    assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
    for text in [
        "MATCH (v:L) RETURN COLLECT(v.p) AS values ORDER BY values",
        "MATCH (v:L) RETURN COLLECT(DISTINCT v.p) AS values LIMIT 0",
    ] {
        // COLLECT has since joined the physical profile. Preserve that newer
        // capability instead of restoring the saved patch's obsolete refusal.
        let q = query(text);
        let rows = fixture(63);
        let expected = eager(&q, &rows);
        assert_eq!(
            run(&q, source(rows), wide())
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected
        );
    }
}
