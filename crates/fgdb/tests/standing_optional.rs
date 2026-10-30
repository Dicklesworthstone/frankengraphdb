//! Maintained OPTIONAL matches compared with public storage and full GLA reads.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, StandingQueryError,
    StandingQueryFailure, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphMatchClause, GraphPatternBuilder, GraphValue,
    IntegerComparison, VertexPredicate,
};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregate, GraphAggregateRow, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const OWNER: LabelId = LabelId(1);
const FLAG: PropertyKeyId = PropertyKeyId(1);
const AMOUNT: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x51; 32],
        DatabaseSecurityNamespaceId([0x52; 32]),
        [0x53; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn definition(direction: GlaDirection, grouping: usize) -> PreparedGraphAggregate {
    let mut root = GraphPatternBuilder::new();
    root.vertex("a").unwrap();
    root.filter("a", VertexPredicate::HasLabel(OWNER)).unwrap();
    let mut child = GraphPatternBuilder::new();
    child.vertex("a").unwrap().vertex("b").unwrap();
    child.edge("a", R, direction, "b").unwrap();
    child
        .filter(
            "b",
            VertexPredicate::IntegerProperty {
                key: FLAG,
                comparison: IntegerComparison::Equal,
                value: 1,
            },
        )
        .unwrap();
    let input = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&child)],
            &[
                GraphColumn::vertex("root", "a"),
                GraphColumn::vertex("child", "b"),
                GraphColumn::property("amount", "b", AMOUNT),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let group = match grouping {
        0 => vec![0],
        1 => vec![1],
        _ => vec![],
    };
    PreparedGraphAggregate::prepare(
        input,
        &group,
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("matches", 1),
            GraphAggregate::count("nonnull", 2),
            GraphAggregate::sum_int("sum", 2),
            GraphAggregate::average_int("average", 2),
        ],
        0,
        None,
    )
    .unwrap()
}

type Key = Option<VId>;
type Summary = (u64, u64, u64, Option<i128>, Option<(i128, u64)>);
// Group key -> every (optional child, optional amount) row in that group.
type GroupedMatches = BTreeMap<Key, Vec<(Option<VId>, Option<i128>)>>;
fn reduced(sum: i128, count: u64) -> (i128, u64) {
    let (mut a, mut b) = (sum.unsigned_abs(), u128::from(count));
    while b != 0 {
        (a, b) = (b, a % b);
    }
    (sum / a as i128, count / a as u64)
}
fn plain(rows: &[GraphAggregateRow]) -> BTreeMap<Key, Summary> {
    rows.iter()
        .map(|row| {
            let key = row.keys().first().and_then(GraphValue::as_vertex);
            (
                key,
                (
                    row.get(0).unwrap().as_count().unwrap(),
                    row.get(1).unwrap().as_count().unwrap(),
                    row.get(2).unwrap().as_count().unwrap(),
                    row.get(3).unwrap().as_integer(),
                    row.get(4)
                        .unwrap()
                        .as_average()
                        .map(|avg| (avg.numerator(), avg.denominator())),
                ),
            )
        })
        .collect()
}
fn oracle(
    db: &Database<MemVfs>,
    direction: GlaDirection,
    grouping: usize,
) -> BTreeMap<Key, Summary> {
    let vertices = db.vertices().unwrap();
    let edges = db.edges().unwrap();
    let mut groups: GroupedMatches = BTreeMap::new();
    for root in vertices.iter().filter(|row| row.labels.contains(&OWNER)) {
        let mut matches = Vec::new();
        for edge in edges.iter().filter(|edge| edge.entry.relation == R) {
            let (src, dst) = (edge.entry.src, edge.entry.dst);
            let other = match direction {
                GlaDirection::Forward if src == root.vid => Some(dst),
                GlaDirection::Reverse if dst == root.vid => Some(src),
                GlaDirection::Undirected if src == root.vid => Some(dst),
                GlaDirection::Undirected if dst == root.vid => Some(src),
                _ => None,
            };
            let Some(child) = other.and_then(|id| vertices.iter().find(|row| row.vid == id)) else {
                continue;
            };
            if !child
                .props
                .iter()
                .any(|(key, value)| *key == FLAG && *value == CanonicalScalar::Int(1).into())
            {
                continue;
            }
            let amount = child.props.iter().find_map(|(key, value)| match value {
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)) if *key == AMOUNT => {
                    Some(i128::from(*n))
                }
                _ => None,
            });
            matches.push((Some(child.vid), amount));
        }
        if matches.is_empty() {
            matches.push((None, None));
        }
        for (child, amount) in matches {
            let key = match grouping {
                0 => Some(root.vid),
                1 => child,
                _ => None,
            };
            groups.entry(key).or_default().push((child, amount));
        }
    }
    if grouping == 2 && groups.is_empty() {
        groups.insert(None, Vec::new());
    }
    groups
        .into_iter()
        .map(|(key, rows)| {
            let values: Vec<_> = rows.iter().filter_map(|(_, value)| *value).collect();
            let sum: i128 = values.iter().sum();
            let count = values.len() as u64;
            (
                key,
                (
                    rows.len() as u64,
                    rows.iter().filter(|(child, _)| child.is_some()).count() as u64,
                    count,
                    (count > 0).then_some(sum),
                    (count > 0).then(|| reduced(sum, count)),
                ),
            )
        })
        .collect()
}

#[test]
fn optional_roots_nulls_parallel_edges_and_cascades_match_both_oracles() {
    let ((), report) = run_async_under_lab(0x8f01, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut queries = Vec::new();
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for grouping in 0..3 {
                let query = definition(direction, grouping);
                let handle = db
                    .register_standing_query(&cx, query.clone(), policy())
                    .unwrap();
                queries.push((direction, grouping, query, handle));
            }
        }
        for step in 0..14 {
            let mut batch = WriteBatch::new(R);
            match step {
                0 => {
                    for (id, owner, flag, amount) in [
                        (1, true, 1, Some(5)),
                        (2, false, 1, Some(7)),
                        (3, false, 1, None),
                        (4, true, 0, Some(0)),
                    ] {
                        batch.create_vertex(
                            VId(id),
                            if owner { vec![OWNER] } else { vec![] },
                            vec![
                                (FLAG, CanonicalScalar::Int(flag)),
                                (
                                    AMOUNT,
                                    amount.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                                ),
                            ],
                        );
                    }
                    for (id, src, dst) in [(1, 1, 2), (2, 1, 2), (3, 1, 3), (4, 1, 1), (5, 2, 1)] {
                        batch.add_edge(EId(id), VId(src), VId(dst), vec![]);
                    }
                }
                1 => {
                    batch.delete_edge(EId(1));
                }
                2 => {
                    batch.set_vertex_property(VId(2), FLAG, Some(CanonicalScalar::Int(0)));
                }
                3 => {
                    batch.set_vertex_property(VId(3), AMOUNT, Some(CanonicalScalar::Int(4)));
                }
                4 => {
                    batch.delete_edge(EId(3));
                    batch.add_edge(EId(6), VId(1), VId(3), vec![]);
                }
                5 => {
                    batch.delete_vertex(VId(3));
                }
                6 => {
                    batch.set_vertex_label(VId(1), OWNER, false);
                }
                7 => {
                    batch.set_vertex_label(VId(1), OWNER, true);
                }
                8 => {
                    batch.add_edge(EId(7), VId(4), VId(2), vec![]);
                    batch.set_vertex_property(VId(2), FLAG, Some(CanonicalScalar::Int(1)));
                }
                9 => {
                    batch.set_vertex_property(VId(1), FLAG, Some(CanonicalScalar::Int(0)));
                }
                10 => {
                    batch.delete_vertex(VId(1));
                }
                11 => {
                    batch.delete_vertex(VId(4));
                }
                12 => {
                    batch.create_vertex(VId(5), vec![OWNER], vec![]);
                }
                13 => {
                    batch.add_edge(EId(8), VId(5), VId(2), vec![]);
                }
                _ => unreachable!(),
            }
            let at = db.write(&commit, batch).await.unwrap();
            for (direction, grouping, query, handle) in &queries {
                let full = db
                    .execute_graph_aggregate_governed(&cx, query, policy())
                    .unwrap()
                    .value;
                assert_eq!(
                    plain(&full),
                    oracle(&db, *direction, *grouping),
                    "step={step} dir={direction:?} grouping={grouping}"
                );
                let view = db.standing_query(&cx, handle).unwrap();
                assert_eq!(view.frontier(), at);
                assert_eq!(view.rows().len(), full.len());
                for row in &full {
                    assert_eq!(view.rows().weight(row), Some(&ZWeight::ONE), "step={step}");
                }
            }
            if step == 4 || step == 10 {
                db.compact(&commit).await.unwrap();
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Owner") => Some(GraphSymbol::Label(OWNER)),
        (GraphSymbolKind::Property, "flag") => Some(GraphSymbol::Property(FLAG)),
        (GraphSymbolKind::Property, "amount") => Some(GraphSymbol::Property(AMOUNT)),
        _ => None,
    }
}

#[test]
fn optional_relationship_fields_and_identities_follow_every_committed_transition() {
    let ((), report) = run_async_under_lab(0x8f17, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let parameters = GqlParameters::new();
        let delivery = GqlQueryPolicy::new(0, 100_000, 10_000_000, 10_000_000);
        let mut definitions = Vec::new();
        for pattern in ["(a)-[e:R]->(b)", "(a)<-[e:R]-(b)", "(a)-[e:R]-(b)"] {
            let prefix = format!("MATCH (a:Owner) OPTIONAL MATCH {pattern} WHERE b.flag=1");
            for projection in [
                "RETURN a,e,e.amount AS amount,b.amount AS endpoint",
                "RETURN e.amount AS amount",
                "WITH a AS root,COUNT(*) AS rows,COUNT(e) AS hits,SUM(e.amount) AS total RETURN root,rows,hits,total ORDER BY root",
                "WITH e.amount AS amount,COUNT(DISTINCT e) AS distinct_edges,COUNT(*) AS copies RETURN amount,distinct_edges,copies ORDER BY amount",
            ] {
                let text = format!("{prefix} {projection}");
                let handle = db
                    .register_standing_native(&cx, &text, &parameters, symbols, policy())
                    .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                definitions.push((text, handle));
            }
        }
        for step in 0..11 {
            let mut change = WriteBatch::new(R);
            match step {
                0 => {
                    for (vid, labels, flag, amount) in [
                        (1, vec![OWNER], 1, 50),
                        (2, vec![OWNER], 1, 80),
                        (3, vec![], 1, 30),
                        (4, vec![OWNER], 0, 40),
                        (5, vec![OWNER], 1, 90),
                    ] {
                        change.create_vertex(
                            VId(vid),
                            labels,
                            vec![
                                (FLAG, CanonicalScalar::Int(flag)),
                                (AMOUNT, CanonicalScalar::Int(amount)),
                            ],
                        );
                    }
                    for (eid, src, dst, amount) in [
                        (0, 1, 3, 2),
                        (u128::MAX, 1, 3, 4),
                        (2, 2, 1, 7),
                        (3, 2, 2, 11),
                        (4, 1, 4, 13),
                    ] {
                        change.add_edge(
                            EId(eid),
                            VId(src),
                            VId(dst),
                            vec![(AMOUNT, CanonicalScalar::Int(amount))],
                        );
                    }
                }
                1 => {
                    // A property-only tick must retract the old edge field.
                    change.set_edge_property(EId(0), AMOUNT, Some(CanonicalScalar::Int(6)));
                }
                2 => {
                    change.set_edge_property(EId(u128::MAX), AMOUNT, None);
                    change.set_vertex_property(VId(3), AMOUNT, Some(CanonicalScalar::Int(31)));
                }
                3 => {
                    // All of root 1's former witnesses become false together.
                    change.set_vertex_property(VId(3), FLAG, Some(CanonicalScalar::Int(0)));
                }
                4 => {
                    // A replacement witness removes the null row only once.
                    change.delete_edge(EId(0));
                    change.add_edge(
                        EId(6),
                        VId(1),
                        VId(5),
                        vec![(AMOUNT, CanonicalScalar::Int(17))],
                    );
                }
                5 => {
                    change.set_vertex_property(VId(3), FLAG, Some(CanonicalScalar::Int(1)));
                    change.set_vertex_label(VId(4), OWNER, false);
                }
                6 => {
                    // Cascades retract fields before the endpoint disappears.
                    change.delete_vertex(VId(1));
                }
                7 => {
                    change.delete_edge(EId(3));
                }
                8 => {
                    change.set_vertex_label(VId(3), OWNER, true);
                    change.add_edge(EId(7), VId(3), VId(5), vec![]);
                }
                9 => {
                    change.delete_vertex(VId(5));
                }
                _ => {
                    for vid in [2, 3, 4] {
                        change.delete_vertex(VId(vid));
                    }
                }
            }
            let at = db.write(&commit, change).await.unwrap();
            for (index, (text, handle)) in definitions.iter().enumerate() {
                let expected = db.query(&cx, text, &parameters, symbols, policy()).unwrap();
                let (frontier, actual) = db.standing_native_query(&cx, handle, delivery).unwrap();
                assert_eq!(frontier, at);
                assert_eq!(actual, expected, "step={step}: {text}");
                let mut cursor = db.standing_native_cursor(&cx, handle, delivery).unwrap();
                let QueryResult::Rows { rows, .. } = actual else {
                    panic!("native rows");
                };
                assert_eq!(
                    cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                    rows
                );
                assert_eq!(cursor.row_stats().snapshot_records, 0);
                drop(cursor);
                if step == 0 && index == 0 {
                    let cell = |value| QueryValue::Value(GraphValue::Scalar(value));
                    let vertex = |vid| QueryValue::Value(GraphValue::Vertex(VId(vid)));
                    let edge = |eid| QueryValue::Value(GraphValue::Edge(EId(eid)));
                    assert_eq!(
                        rows,
                        vec![
                            vec![
                                vertex(1),
                                edge(0),
                                cell(CanonicalScalar::Int(2)),
                                cell(CanonicalScalar::Int(30))
                            ],
                            vec![
                                vertex(1),
                                edge(u128::MAX),
                                cell(CanonicalScalar::Int(4)),
                                cell(CanonicalScalar::Int(30))
                            ],
                            vec![
                                vertex(2),
                                edge(2),
                                cell(CanonicalScalar::Int(7)),
                                cell(CanonicalScalar::Int(50))
                            ],
                            vec![
                                vertex(2),
                                edge(3),
                                cell(CanonicalScalar::Int(11)),
                                cell(CanonicalScalar::Int(80))
                            ],
                            vec![
                                vertex(4),
                                cell(CanonicalScalar::Null),
                                cell(CanonicalScalar::Null),
                                cell(CanonicalScalar::Null)
                            ],
                            vec![
                                vertex(5),
                                cell(CanonicalScalar::Null),
                                cell(CanonicalScalar::Null),
                                cell(CanonicalScalar::Null)
                            ],
                        ],
                    );
                }
                if matches!(step, 2 | 6 | 10) {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                    assert_eq!(
                        db.standing_native_query(&cx, handle, delivery).unwrap().1,
                        expected,
                        "rebuild step={step}: {text}"
                    );
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_edge_field_ticks_touch_only_the_changed_relationship() {
    let ((), report) = run_async_under_lab(0x8f18, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let text = "MATCH (a:Owner) OPTIONAL MATCH (a)-[e:R]->(b) RETURN a,COUNT(e) AS hits,SUM(e.amount) AS total GROUP BY a";
        let query = parsed(text);
        let mut measurements = Vec::new();
        for size in [1, 1000] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![OWNER], vec![]);
            seed.create_vertex(VId(2), vec![], vec![]);
            seed.create_vertex(VId(3), vec![OWNER], vec![]);
            for eid in 1..=size {
                seed.add_edge(
                    EId(eid),
                    VId(1),
                    VId(2),
                    vec![(AMOUNT, CanonicalScalar::Int(2))],
                );
            }
            db.write(&commit, seed).await.unwrap();
            let handle = db
                .register_standing_query(&cx, query.clone(), policy())
                .unwrap();
            let mut update = WriteBatch::new(R);
            update.set_edge_property(EId(1), AMOUNT, Some(CanonicalScalar::Int(9)));
            let at = db.write(&commit, update).await.unwrap();
            let expected = db
                .execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value;
            let view = db.standing_query(&cx, &handle).unwrap();
            assert_eq!(view.frontier(), at);
            assert_eq!(view.rows().len(), expected.len());
            for row in expected {
                assert_eq!(view.rows().weight(&row), Some(&ZWeight::ONE));
            }
            measurements.push(*view.last_maintenance());
            assert_eq!(measurements.last().unwrap().affected_edges, 1);

            // Derived failure fences the view, never the authoritative commit
            // or a partially updated aggregate. A rebuild reads the repaired
            // committed field through the same ordinary snapshot path.
            let mut invalid = WriteBatch::new(R);
            invalid.set_edge_property(
                EId(1),
                AMOUNT,
                Some(CanonicalScalar::ucs_basic_text("not numeric").unwrap()),
            );
            let failed_at = db.write(&commit, invalid).await.unwrap();
            assert_eq!(db.frontier().unwrap(), failed_at);
            assert!(matches!(
                db.standing_query(&cx, &handle),
                Err(StandingQueryError::Unavailable {
                    reason: StandingQueryFailure::NonIntegerSum,
                    ..
                })
            ));
            assert!(
                db.execute_graph_aggregate_governed(&cx, &query, policy())
                    .is_err()
            );
            let mut repair = WriteBatch::new(R);
            repair.set_edge_property(EId(1), AMOUNT, Some(CanonicalScalar::Int(12)));
            let repaired_at = db.write(&commit, repair).await.unwrap();
            assert_eq!(
                db.rebuild_standing_query(&cx, &handle, policy()).unwrap(),
                repaired_at
            );
            let full = db
                .execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value;
            let view = db.standing_query(&cx, &handle).unwrap();
            assert_eq!(view.rows().len(), full.len());
            for row in full {
                assert_eq!(view.rows().weight(&row), Some(&ZWeight::ONE));
            }
        }
        assert_eq!(measurements[0], measurements[1]);
        assert!(measurements[0].work_units > 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
fn parsed(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

#[test]
fn parsed_child_predicates_and_repeated_root_identity_do_not_filter_null_extension() {
    let ((), report) = run_async_under_lab(0x8f02, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let texts = [
            "MATCH (a:Owner) OPTIONAL MATCH (a)-[:R]->(b) WHERE a.amount < b.amount RETURN a,COUNT(*) AS rows,COUNT(b) AS hits GROUP BY a",
            "MATCH (a:Owner) OPTIONAL MATCH (a)-[:R]->(a) RETURN a,COUNT(*) AS rows GROUP BY a",
            "MATCH (a:Owner) OPTIONAL MATCH (b)-[:R]->(a) WHERE b.flag=1 RETURN a,COUNT(b) AS hits GROUP BY a",
        ];
        let mut queries = Vec::new();
        for text in texts {
            let query = parsed(text);
            let handle = db
                .register_standing_query(&cx, query.clone(), policy())
                .unwrap();
            queries.push((query, handle));
        }
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![OWNER], vec![(AMOUNT, CanonicalScalar::Int(5))]);
        seed.create_vertex(
            VId(2),
            vec![],
            vec![
                (AMOUNT, CanonicalScalar::Int(9)),
                (FLAG, CanonicalScalar::Int(1)),
            ],
        );
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        seed.add_edge(EId(2), VId(2), VId(1), vec![]);
        db.write(&commit, seed).await.unwrap();
        for step in 0..5 {
            let mut batch = WriteBatch::new(R);
            match step {
                0 => {
                    batch.set_vertex_property(VId(2), AMOUNT, Some(CanonicalScalar::Int(10)));
                }
                1 => {
                    batch.set_vertex_property(VId(1), AMOUNT, Some(CanonicalScalar::Int(20)));
                }
                2 => {
                    batch.set_vertex_property(VId(2), AMOUNT, Some(CanonicalScalar::Null));
                }
                3 => {
                    batch.add_edge(EId(3), VId(1), VId(1), vec![]);
                }
                _ => {
                    batch.delete_vertex(VId(2));
                }
            }
            db.write(&commit, batch).await.unwrap();
            for (query, handle) in &queries {
                let full = db
                    .execute_graph_aggregate_governed(&cx, query, policy())
                    .unwrap()
                    .value;
                let view = db.standing_query(&cx, handle).unwrap();
                assert_eq!(view.rows().len(), full.len());
                for row in full {
                    assert_eq!(view.rows().weight(&row), Some(&ZWeight::ONE));
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_maintenance_is_committed_only_and_rebuilds_after_result_refusal() {
    let ((), report) = run_async_under_lab(0x8f03, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let query = definition(GlaDirection::Forward, 0);
        let handle = db
            .register_standing_query(
                &cx,
                query.clone(),
                GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000),
            )
            .unwrap();
        let mut tx = db.begin(&contexts.txn()).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![OWNER], vec![]);
        tx.write(&mut db, batch).unwrap();
        assert!(db.standing_query(&cx, &handle).unwrap().rows().is_empty());
        tx.commit(&mut db, &commit).await.unwrap();
        assert_eq!(db.standing_query(&cx, &handle).unwrap().rows().len(), 1);
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(2), vec![OWNER], vec![]);
        let at = db.write(&commit, batch).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(matches!(
            db.standing_query(&cx, &handle),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::ResultBudget,
                ..
            })
        ));
        assert_eq!(
            db.rebuild_standing_query(&cx, &handle, policy()).unwrap(),
            at
        );
        assert_eq!(db.standing_query(&cx, &handle).unwrap().rows().len(), 2);
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_query(&cx, &handle),
            Err(StandingQueryError::ForeignHandle)
        ));
        let handle = db
            .register_standing_query(&cx, query.clone(), policy())
            .unwrap();
        let full = db
            .execute_graph_aggregate_governed(&cx, &query, policy())
            .unwrap()
            .value;
        for row in full {
            assert_eq!(
                db.standing_query(&cx, &handle).unwrap().rows().weight(&row),
                Some(&ZWeight::ONE)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn replacing_one_witness_does_not_revisit_the_roots_other_witnesses() {
    let ((), report) = run_async_under_lab(0x8f04, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut measurements = Vec::new();
        for size in [1, 1000] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![OWNER], vec![]);
            seed.create_vertex(
                VId(2),
                vec![],
                vec![
                    (FLAG, CanonicalScalar::Int(1)),
                    (AMOUNT, CanonicalScalar::Int(7)),
                ],
            );
            for eid in 1..=size {
                seed.add_edge(EId(eid), VId(1), VId(2), vec![]);
            }
            db.write(&commit, seed).await.unwrap();
            let query = definition(GlaDirection::Forward, 0);
            let handle = db.register_standing_query(&cx, query, policy()).unwrap();
            let before: Vec<_> = db
                .standing_query(&cx, &handle)
                .unwrap()
                .rows()
                .iter()
                .map(|(row, _)| row.clone())
                .collect();
            let mut replacement = WriteBatch::new(R);
            replacement.delete_edge(EId(1));
            replacement.add_edge(EId(size + 1), VId(1), VId(2), vec![]);
            let at = db.write(&commit, replacement).await.unwrap();
            let view = db.standing_query(&cx, &handle).unwrap();
            assert_eq!(view.frontier(), at);
            assert_eq!(view.rows().len(), before.len());
            for row in before {
                assert_eq!(view.rows().weight(&row), Some(&ZWeight::ONE));
            }
            assert_eq!(view.last_maintenance().affected_edges, 2);
            measurements.push(*view.last_maintenance());
        }
        assert_eq!(measurements[0], measurements[1]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_scope_shapes_refuse_instead_of_becoming_inner_joins() {
    let ((), report) = run_async_under_lab(0x8f06, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        for text in [
            "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) OPTIONAL MATCH (b)-[:R]->(c) RETURN COUNT(*) AS rows",
            "MATCH (a) OPTIONAL MATCH (b)-[:R]->(c) RETURN COUNT(*) AS rows",
            "MATCH (a) OPTIONAL MATCH WALK (a)-[:R*1..2]->(b) RETURN COUNT(*) AS rows",
        ] {
            assert!(
                matches!(
                    db.register_standing_query(&contexts.query(), parsed(text), policy()),
                    Err(StandingQueryError::Unsupported)
                ),
                "{text}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
