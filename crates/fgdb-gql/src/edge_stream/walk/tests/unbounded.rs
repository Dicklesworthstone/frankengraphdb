use super::*;

#[test]
fn terminating_open_routes_match_complete_finite_oracles_and_native_execution() {
    for mask in [0, 1, 3, 13, 31] {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for (search, different) in [
                (GraphWalkSearch::All, true),
                (GraphWalkSearch::Trail, false),
                (GraphWalkSearch::Trail, true),
                (GraphWalkSearch::Acyclic, false),
                (GraphWalkSearch::Acyclic, true),
                (GraphWalkSearch::Simple, false),
                (GraphWalkSearch::Simple, true),
            ] {
                for minimum in [0, 2, 6] {
                    let source = Source::new(mask);
                    // Every trail spends a distinct EId; every simple route
                    // spends a distinct vertex before its optional closure.
                    // This finite oracle covers every possible valid route,
                    // including a lower bound beyond all of them.
                    let maximum = (source.data.edges.len().max(source.data.vertices.len()) as u32)
                        .max(minimum);
                    let finite = statement(direction, minimum, maximum);
                    let text = finite.replacen(&format!("..{maximum}]"), "..]", 1);
                    let logical = definition(&text, search, different);
                    let expected =
                        oracle(&source.data, direction, minimum, maximum, search, different);
                    assert_eq!(
                        native_with_edge_properties(&logical, &source.data),
                        expected,
                        "native {text}, mask={mask}, {search:?}, different={different}"
                    );
                    assert_eq!(
                        drain(definition(&finite, search, different), Source::new(mask)),
                        expected,
                        "bounded control {finite}, mask={mask}, {search:?}, different={different}"
                    );
                    let counts = source.counts.clone();
                    assert_eq!(
                        drain(logical, source),
                        expected,
                        "open {text}, mask={mask}, {search:?}, different={different}"
                    );
                    assert!(counts.traversal_peak_hops.load(Ordering::SeqCst) <= 14);
                    assert_eq!(counts.traversal_hops.load(Ordering::SeqCst), 0);
                }
            }
        }
    }
}

#[test]
fn open_routes_keep_exact_limits_cancellation_and_the_unrestricted_walk_refusal() {
    for minimum in [0, 2] {
        let text = format!("MATCH p = (a {{p:1}})-[:R*{minimum}..]->(b) RETURN p LIMIT 0");
        let logical = prepared(&text);
        let (plan, tail) = AsyncGraphWalkPlan::compile_sort_input(logical.plan()).unwrap();
        assert!(!plan.bounds.is_bounded());
        assert_eq!(tail.count(), Some(0));
        check_all_live_limits(plan, || Source::new(13));

        // A typed caller cannot erase the termination proof and smuggle an
        // exhaustive repeatable WALK through an outer empty page.
        let repeatable = definition(&text, GraphWalkSearch::All, false);
        assert!(AsyncGraphWalkPlan::compile_sort_input(&repeatable).is_err());
    }
}

#[test]
fn route_growth_reserves_replacement_before_refunding_old_backing() {
    let logical = prepared("MATCH p = (a {p:1})-[:R*0..]->(b) RETURN p");
    let plan = AsyncGraphWalkPlan::compile_sort_input(logical.plan())
        .unwrap()
        .0;
    let mut source = Source::new(13);
    // The root (one slot) and first replacement (two slots) fit. The next
    // replacement needs three slots while its predecessor still owns two.
    // Merely charging the final capacity would incorrectly admit it.
    source.traversal_limit = 4;
    let counts = source.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
    let first = run(cursor.next()).unwrap().unwrap();
    assert!(matches!(&first.values()[0], GraphValue::Path(path) if path.is_empty()));
    let second = run(cursor.next()).unwrap().unwrap();
    assert!(matches!(&second.values()[0], GraphValue::Path(path) if path.len() == 1));
    drop(second);
    assert!(matches!(
        run(cursor.next()),
        Some(Err(GqlQueryError::Source(EdgeScanError::Source(
            "route backing limit"
        ))))
    ));
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.traversal_peak_hops.load(Ordering::SeqCst), 3);
    assert_eq!(counts.traversal_hops.load(Ordering::SeqCst), 0);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 1);
    drop(first);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn dropping_a_pending_open_route_releases_its_grown_backing() {
    let logical = prepared("MATCH p = (a {p:1})-[:R*0..]->(b) RETURN p");
    let plan = AsyncGraphWalkPlan::compile_sort_input(logical.plan())
        .unwrap()
        .0;
    let mut source = Source::new(13);
    source.pending_after = 2;
    let counts = source.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
    for depth in 0..=2 {
        let row = run(cursor.next()).unwrap().unwrap();
        assert!(matches!(&row.values()[0], GraphValue::Path(path) if path.len() == depth));
    }
    assert!(counts.traversal_hops.load(Ordering::SeqCst) > 0);
    let mut future = Box::pin(cursor.next::<usize>());
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&future);
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(future);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.traversal_hops.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn open_routes_can_pass_the_finite_definition_hop_limit() {
    let hops = crate::MAX_GRAPH_WALK_HOPS as usize + 1;
    let mut source = Source::new(0);
    let data = Arc::get_mut(&mut source.data).unwrap();
    for at in 1..=hops {
        let vid = VId(at as u128);
        data.vertices.insert(
            vid,
            vec![(
                P,
                CanonicalScalar::Int(if at == hops { 99 } else { 10 }).into(),
            )],
        );
        data.edges.push((EId(at as u128), VId(at as u128 - 1), vid));
    }
    let logical = prepared("MATCH p = (a {p:1})-[:R*]->(b {p:99}) RETURN p");
    let expected = vec![vec![GraphValue::Path(GraphPath::new(
        IDS[0],
        (1..=hops)
            .map(|at| (EId(at as u128), VId(at as u128)))
            .collect(),
    ))]];
    assert_eq!(
        native_with_edge_properties(logical.plan(), &source.data),
        expected
    );
    let counts = source.counts.clone();
    assert_eq!(drain(logical.plan().clone(), source), expected);
    assert!(counts.traversal_peak_hops.load(Ordering::SeqCst) < 4 * hops);
    assert_eq!(counts.traversal_hops.load(Ordering::SeqCst), 0);
}
