//! Independent MATCH components use the same bounded evaluator as connected
//! patterns. Oracles enumerate raw assignments, not the compiler's slot order.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GlaOperator, GraphColumn, GraphMatchClause,
    GraphPatternBuilder, GraphValueRow, PatternBuildError, PatternLimitDimension,
    PreparedGraphPattern, VertexPredicate, MAX_PATTERN_BINDINGS, MAX_PATTERN_VERTICES};
use fgdb_gql::{GlaExecutionEvent, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

type Vertices = BTreeMap<VId, (Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>)>;
type Edge = (VId, RelationId, VId);
const CIK: PropertyKeyId = PropertyKeyId(1);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Label, "Company") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "Filing") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Label, "Audit") => Some(GraphSymbol::Label(LabelId(3))),
        (GraphSymbolKind::Label, "Tag") => Some(GraphSymbol::Label(LabelId(4))),
        (GraphSymbolKind::Property, "cik") => Some(GraphSymbol::Property(CIK)),
        _ => None,
    }
}
fn query(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn fixture() -> Vertices {
    let mut vertices = Vertices::new();
    for (id, label, cik) in [(1, 1, Some(10)), (2, 1, Some(20)), (3, 1, None),
        (10, 2, Some(10)), (11, 2, Some(10)), (12, 2, Some(20)), (13, 2, None),
        (14, 2, None), (20, 3, Some(10)), (30, 4, Some(10))] {
        vertices.insert(VId(id), (vec![LabelId(label)],
            cik.map(|value| vec![(CIK, CanonicalScalar::Int(value))]).unwrap_or_default()));
    }
    vertices.get_mut(&VId(13)).unwrap().1.push((CIK, CanonicalScalar::Null));
    vertices
}
fn matches(vertices: &Vertices, vid: VId, predicates: &[VertexPredicate]) -> bool {
    vertices.get(&vid).is_some_and(|(labels, props)|
        predicates.iter().all(|predicate| predicate.matches(labels, props)))
}
fn property(vertices: &Vertices, vid: VId, key: PropertyKeyId) -> Option<&CanonicalScalar> {
    vertices.get(&vid)?.1.iter().find(|(actual, _)| *actual == key).map(|(_, value)| value)
}
fn run(pattern: &PreparedGraphPattern<GraphValueRow>, vertices: &Vertices, edges: &[Edge]) -> Vec<GraphValueRow> {
    pattern.plan().execute_with_properties_control(
        vertices.keys().copied(), edges.iter().copied(),
        |vid, predicates| Ok::<_, ()>(matches(vertices, vid, predicates)),
        |vid, key| Ok(property(vertices, vid, key)), |_| Ok(()),
    ).unwrap()
}
fn ids(rows: &[GraphValueRow]) -> Vec<Vec<Option<VId>>> {
    rows.iter().map(|row| row.values().iter().map(|value| value.as_vertex()).collect()).collect()
}
fn some(values: &[u128]) -> Vec<Option<VId>> {
    values.iter().map(|value| Some(VId(*value))).collect()
}

#[test]
fn isolated_property_joins_preserve_correlation_nulls_bags_and_pagination() {
    let vertices = fixture();
    let head = "MATCH (company:Company),(filing:Filing) WHERE company.cik = filing.cik";
    let pattern = query(&format!("{head} RETURN company,filing"));
    assert_eq!(pattern.required_vertex_label(), None);
    assert_eq!(ids(&run(&pattern, &vertices, &[])), vec![some(&[1, 10]), some(&[1, 11]), some(&[2, 12])]);
    assert_eq!(ids(&run(&query(&format!("{head} RETURN company")), &vertices, &[])),
        vec![some(&[1]), some(&[1]), some(&[2])]);
    assert_eq!(ids(&run(&query(&format!("{head} RETURN DISTINCT company")), &vertices, &[])),
        vec![some(&[1]), some(&[2])]);
    assert_eq!(ids(&run(&query(&format!("{head} RETURN company SKIP 1 LIMIT 1")), &vertices, &[])),
        vec![some(&[1])]);
    let boolean = query("MATCH (company:Company),(filing:Filing) \
        WHERE NOT (company.cik <> filing.cik) RETURN company,filing");
    assert_eq!(run(&boolean, &vertices, &[]), run(&pattern, &vertices, &[]));
    assert!(run(&pattern, &Vertices::new(), &[]).is_empty());
    let frozen = pattern.canonical_bytes();
    let renamed = query("MATCH (x:Company),(y:Filing) WHERE x.cik = y.cik RETURN x,y");
    assert_eq!(frozen, renamed.canonical_bytes());
}

#[test]
fn independent_sources_are_consumed_once_and_failures_are_not_partial_rows() {
    let vertices = fixture();
    let pattern = query("MATCH (company:Company),(filing:Filing) \
        WHERE company.cik = filing.cik RETURN company,filing");
    let mut admitted = 0;
    let actual = pattern.plan().execute_with_properties_control(
        vertices.keys().copied().inspect(|_| admitted += 1), [],
        |vid, predicates| Ok::<_, &'static str>(matches(&vertices, vid, predicates)),
        |vid, key| Ok(property(&vertices, vid, key)), |_| Ok(()),
    ).unwrap();
    assert_eq!(admitted, vertices.len());
    assert_eq!(actual.len(), 3);
    let failed = pattern.plan().execute_with_properties_control(
        vertices.keys().copied(), [],
        |vid, predicates| Ok(matches(&vertices, vid, predicates)),
        |vid, key| if vid == VId(12) { Err("unreadable independent vertex") }
            else { Ok(property(&vertices, vid, key)) }, |_| Ok(()),
    );
    assert_eq!(failed, Err("unreadable independent vertex"));
}

#[test]
fn disconnected_edge_components_and_isolates_match_exhaustive_assignment_bags() {
    type Atom = (usize, u64, u8, usize);
    type Case<'a> = (&'a str, &'a [Atom], usize, &'a [usize], bool, bool);
    let cases: [Case<'_>; 3] = [
        ("MATCH (a)-[:R]->(b),(c)-[:S]->(d),(e) WHERE a <> e RETURN a,b,c,d,e",
            &[(0, 1, 0, 1), (2, 2, 0, 3)], 5, &[0, 1, 2, 3, 4], false, false),
        ("MATCH (a)<-[:R]-(b),(c)-[:S]-(d),(e) WHERE a <> e \
            RETURN DISTINCT e,d,c,b,a SKIP 1 LIMIT 5",
            &[(0, 1, 1, 1), (2, 2, 2, 3)], 5, &[4, 3, 2, 1, 0], true, true),
        ("MATCH (a)-[:R]-(b),(c)-[:S]->(c),(e) WHERE a <> e RETURN a,b,c,e",
            &[(0, 1, 2, 1), (2, 2, 0, 2)], 4, &[0, 1, 2, 3], false, false),
    ];
    let patterns: Vec<_> = cases.iter().map(|case| query(case.0)).collect();
    let vertices: Vertices = (1..=3).map(|id| (VId(id), (vec![], vec![]))).collect();
    let universe: Vec<_> = (1..=2).flat_map(|relation| (1..=2).flat_map(move |source|
        (1..=2).map(move |target| (VId(source), RelationId(relation), VId(target))))).collect();
    for mask in 0..256_usize {
        let mut edges: Vec<_> = universe.iter().enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0).map(|(_, edge)| *edge).collect();
        if let Some(first) = edges.first().copied() { edges.push(first); }
        for ((text, atoms, width, selected, distinct, paged), pattern) in cases.iter().zip(&patterns) {
            assert!(!pattern.plan().scans_edges(), "independent components require both domains");
            let mut expected = Vec::new();
            for mut encoding in 0..3_usize.pow(*width as u32) {
                let assignment: Vec<_> = (0..*width).map(|_| {
                    let value = VId((encoding % 3 + 1) as u128); encoding /= 3; value
                }).collect();
                if assignment[0] == assignment[width - 1] { continue; }
                let multiplicity = atoms.iter().map(|&(left, relation, direction, right)| {
                    edges.iter().filter(|&&(s, r, d)| {
                        if r != RelationId(relation) { return false; }
                        let (a, b) = (assignment[left], assignment[right]);
                        match direction {
                            0 => s == a && d == b,
                            1 => d == a && s == b,
                            _ => (s == a && d == b) || (s == b && d == a),
                        }
                    }).count()
                }).product::<usize>();
                for _ in 0..multiplicity {
                    expected.push(selected.iter().map(|at| Some(assignment[*at])).collect::<Vec<_>>());
                }
            }
            expected.sort();
            if *distinct { expected.dedup(); }
            if *paged { expected = expected.into_iter().skip(1).take(5).collect(); }
            assert_eq!(ids(&run(pattern, &vertices, &edges)), expected, "mask={mask}, {text}");
        }
    }
}

#[test]
fn optional_frames_after_disconnected_outer_and_children_keep_null_correlations() {
    let vertices = fixture();
    let pattern = query("MATCH (company:Company),(filing:Filing) \
        WHERE company.cik = filing.cik \
        OPTIONAL MATCH (audit:Audit),(filing) WHERE audit.cik = filing.cik \
        OPTIONAL MATCH (tag:Tag),(audit) WHERE tag.cik = audit.cik \
        RETURN company,filing,audit,tag");
    assert_eq!(ids(&run(&pattern, &vertices, &[])), vec![
        some(&[1, 10, 20, 30]), some(&[1, 11, 20, 30]),
        vec![Some(VId(2)), Some(VId(12)), None, None],
    ]);
    assert_eq!(pattern.required_vertex_label(), None);
    let positive = query("MATCH (company:Company),(filing:Filing) \
        WHERE company.cik = filing.cik AND EXISTS { \
            MATCH (audit:Audit),(filing) WHERE audit.cik = filing.cik } RETURN company,filing");
    let negative = query("MATCH (company:Company),(filing:Filing) \
        WHERE company.cik = filing.cik AND NOT EXISTS { \
            MATCH (audit:Audit),(filing) WHERE audit.cik = filing.cik } RETURN company,filing");
    assert_eq!(ids(&run(&positive, &vertices, &[])), vec![some(&[1, 10]), some(&[1, 11])]);
    assert_eq!(ids(&run(&negative, &vertices, &[])), vec![some(&[2, 12])]);
}

#[test]
fn closing_endpoints_and_isolated_child_anchors_do_not_alias_later_slots() {
    let vertices = fixture();
    let pattern = query("MATCH (company:Company)-[:R]->(company),(filing:Filing) \
        WHERE company.cik = filing.cik OPTIONAL MATCH (audit:Audit)-[:S]->(tag:Tag),(filing) \
        WHERE audit.cik = filing.cik RETURN company,filing,audit,tag");
    let edges = [(VId(1), RelationId(1), VId(1)), (VId(2), RelationId(1), VId(2)),
        (VId(20), RelationId(2), VId(30))];
    assert_eq!(ids(&run(&pattern, &vertices, &edges)), vec![some(&[1, 10, 20, 30]),
        some(&[1, 11, 20, 30]), vec![Some(VId(2)), Some(VId(12)), None, None]]);
}

#[test]
fn independent_walk_component_keeps_zero_hop_isolates_and_parallel_occurrences() {
    let vertices = fixture();
    let pattern = query("MATCH WALK (company:Company),(filing:Filing)-[:R*0..1]->(other) \
        WHERE company.cik = filing.cik RETURN company,filing,other");
    let edges = [(VId(10), RelationId(1), VId(20)); 2];
    assert_eq!(ids(&run(&pattern, &vertices, &edges)), vec![some(&[1, 10, 10]),
        some(&[1, 10, 20]), some(&[1, 10, 20]), some(&[1, 11, 11]), some(&[2, 12, 12])]);
}

#[test]
fn independent_scans_respect_exact_limits_and_every_interruption_checkpoint() {
    let vertices = fixture();
    let pattern = query("MATCH (company:Company),(filing:Filing) \
        WHERE company.cik = filing.cik OPTIONAL MATCH (audit:Audit),(filing) \
        WHERE audit.cik = filing.cik RETURN company,filing,audit");
    let run = |policy| pattern.plan().execute_governed_with_properties(
        vertices.len() as u64, vertices.keys().copied(), [],
        |vid, predicates| Ok::<_, ()>(matches(&vertices, vid, predicates)),
        |vid, key| Ok(property(&vertices, vid, key)), policy, || Ok::<_, usize>(()),
    );
    let measured = run(wide()).unwrap();
    let work = measured.evaluator.work_units;
    let scratch = measured.evaluator.scratch_entries;
    let rows = measured.value.len() as u64;
    let count = vertices.len() as u64;
    assert_eq!(run(GqlQueryPolicy::new(count, rows, work, scratch)).unwrap(), measured);
    for cap in [GqlQueryPolicy::new(count - 1, rows, work, scratch),
        GqlQueryPolicy::new(count, rows - 1, work, scratch),
        GqlQueryPolicy::new(count, rows, work - 1, scratch),
        GqlQueryPolicy::new(count, rows, work, scratch - 1)] {
        assert!(run(cap).is_err());
    }
    let mut total = 0;
    pattern.plan().execute_governed_with_properties(
        count, vertices.keys().copied(), [],
        |vid, predicates| Ok::<_, ()>(matches(&vertices, vid, predicates)),
        |vid, key| Ok(property(&vertices, vid, key)), wide(), || {
            total += 1; Ok::<_, usize>(())
        },
    ).unwrap();
    for stop in 1..=total {
        let mut seen = 0;
        let result = pattern.plan().execute_governed_with_properties(
            count, vertices.keys().copied(), [],
            |vid, predicates| Ok::<_, ()>(matches(&vertices, vid, predicates)),
            |vid, key| Ok(property(&vertices, vid, key)), wide(), || {
                seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
}

#[test]
fn independent_existence_stops_at_first_witness_and_never_converts_failure_to_absence() {
    let pattern = query("MATCH (company:Company) WHERE EXISTS { \
        MATCH (company),(filing:Filing) } RETURN company");
    let mut filing_tests = 0;
    let rows = pattern.plan().execute_with_properties_control([VId(1), VId(2)], [],
        |vid, predicates| {
            if predicates == [VertexPredicate::HasLabel(LabelId(1))] { return Ok(vid == VId(1)); }
            filing_tests += 1;
            if filing_tests == 1 { Ok(true) } else { Err("examined after witness") }
        }, |_, _| Ok(None), |_| Ok(()),
    ).unwrap();
    assert_eq!(ids(&rows), vec![some(&[1])]);
    assert_eq!(filing_tests, 1);
    let anti = query("MATCH (company:Company) WHERE NOT EXISTS { \
        MATCH (company),(filing:Filing) } RETURN company");
    let failed = anti.plan().execute_with_properties_control([VId(1)], [],
        |_, predicates| if predicates == [VertexPredicate::HasLabel(LabelId(1))] { Ok(true) }
            else { Err("source failed") }, |_, _| Ok(None), |_| Ok(()),
    );
    assert_eq!(failed, Err("source failed"));
    let mut events = 0;
    let stopped = anti.plan().execute_with_properties_control([VId(1)], [],
        |_, _| Ok(true), |_, _| Ok(None), |event| {
            events += 1;
            if event == GlaExecutionEvent::ScratchEntry { Err("scratch refused") } else { Ok(()) }
        },
    );
    assert_eq!(stopped, Err("scratch refused"));
    assert!(events > 0);
}

#[test]
fn full_independent_width_and_definition_wide_scope_bounds_are_enforced() {
    let mut root = GraphPatternBuilder::new(); root.vertex("anchor").unwrap();
    let mut full = GraphPatternBuilder::new();
    for index in 0..MAX_PATTERN_VERTICES { full.vertex(&format!("v{index}")).unwrap(); }
    let names: Vec<_> = (0..MAX_PATTERN_VERTICES).map(|index| format!("v{index}")).collect();
    let refs: Vec<_> = names.iter().map(String::as_str).collect();
    let full = full.prepare_bindings(&refs, 0, None).unwrap();
    let rows = full.plan().execute([VId(9)], [], |_, _| Ok::<_, ()>(true)).unwrap();
    assert_eq!(rows[0].values(), vec![VId(9); MAX_PATTERN_VERTICES]);
    assert_eq!(full.plan().operators().iter().filter(|op| matches!(op, GlaOperator::ScanVertices)).count(), MAX_PATTERN_VERTICES);
    let mut child = GraphPatternBuilder::new(); child.vertex("anchor").unwrap();
    for index in 1..64 { child.vertex(&format!("inner{index}")).unwrap(); }
    let clauses = [GraphMatchClause::exists(&child), GraphMatchClause::exists(&child), GraphMatchClause::exists(&child)];
    assert_eq!(1 + 3 * 64, MAX_PATTERN_BINDINGS);
    let columns = [GraphColumn::vertex("anchor", "anchor")];
    let exact = root.prepare_values_with_clauses(&clauses, &columns, 0, None).unwrap();
    assert_eq!(ids(&run(&exact, &BTreeMap::from([(VId(1), (vec![], vec![]))]), &[])), vec![some(&[1])]);
    let mut one_more = GraphPatternBuilder::new(); one_more.vertex("anchor").unwrap();
    let overflowing = [GraphMatchClause::exists(&child), GraphMatchClause::exists(&child),
        GraphMatchClause::exists(&child), GraphMatchClause::exists(&one_more)];
    assert!(matches!(root.prepare_values_with_clauses(&overflowing, &columns, 0, None),
        Err(PatternBuildError::LimitExceeded { dimension: PatternLimitDimension::Bindings,
            limit: MAX_PATTERN_BINDINGS, observed }) if observed == MAX_PATTERN_BINDINGS + 1));
    let mut uncorrelated = GraphPatternBuilder::new(); uncorrelated.vertex("other").unwrap();
    let independent = root.prepare_values_with_clauses(
        &[GraphMatchClause::exists(&uncorrelated)], &columns, 0, None,
    ).unwrap();
    assert_eq!(ids(&run(&independent, &BTreeMap::from([(VId(1), (vec![], vec![]))]), &[])), vec![some(&[1])]);
    let mut connected = GraphPatternBuilder::new(); connected.vertex("a").unwrap(); connected.vertex("b").unwrap();
    connected.edge("a", RelationId(1), GlaDirection::Forward, "b").unwrap();
    assert!(connected.prepare("a", 0, None).unwrap().plan().scans_edges());
}
