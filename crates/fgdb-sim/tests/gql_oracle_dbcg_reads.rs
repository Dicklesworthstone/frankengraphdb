//! Independent differentials for anonymous nodes, bounded walks and hidden ordering.
//! Expected rows come only from public storage scans, not GQL evaluation.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cmp::Ordering;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const NAME: PropertyKeyId = PropertyKeyId(1);
const AGE: PropertyKeyId = PropertyKeyId(2);
const CITY: PropertyKeyId = PropertyKeyId(3);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "KNOWS") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "age") => Some(GraphSymbol::Property(AGE)),
        (GraphSymbolKind::Property, "city") => Some(GraphSymbol::Property(CITY)),
        _ => None,
    }
}

#[test]
fn anonymous_intermediate_nodes_match_independent_reference() {
    let ((), report) = run_async_under_lab(0xD001, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let dir =
            std::env::temp_dir().join(format!("fgdb-oracle-dbcg-minimal-{}", std::process::id()));
        let keys = DatabaseKeys::new(
            [0x6b; 32],
            DatabaseSecurityNamespaceId([0x78; 32]),
            [0x3d; 32],
        );
        let mut db = Database::create(&commit, &dir, keys)
            .await
            .expect("oracle database");
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![PERSON], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.create_vertex(VId(4), vec![], vec![]);
        batch.add_edge(EId(1), VId(1), VId(2), vec![]);
        batch.add_edge(EId(2), VId(2), VId(3), vec![]);
        batch.add_edge(EId(3), VId(4), VId(2), vec![]);
        let as_of = db.write(&commit, batch).await.expect("fixture commit");

        // Each edge pair contributes a row; no endpoint or row deduplication.
        // The scans select visible versions. Only the source has a label test.
        let vertices = db.vertices_at(as_of).expect("vertex scan");
        let edges = db.edges_at(as_of).expect("edge scan");
        let mut pairs = Vec::new();
        for first in edges.iter().filter(|edge| edge.entry.relation == R) {
            let Some(source) = vertices.iter().find(|v| v.vid == first.entry.src) else {
                continue;
            };
            if !source.labels.contains(&PERSON)
                || !vertices.iter().any(|v| v.vid == first.entry.dst)
            {
                continue;
            }
            for second in edges
                .iter()
                .filter(|edge| edge.entry.relation == R && edge.entry.src == first.entry.dst)
            {
                if vertices.iter().any(|v| v.vid == second.entry.dst) {
                    pairs.push((first.entry.src, second.entry.dst));
                }
            }
        }
        pairs.sort_unstable();
        assert_eq!(
            pairs,
            vec![(VId(1), VId(3))],
            "fixture has a two-hop match with an unlabeled intermediate, not an unlabeled source"
        );
        let expected: Vec<Vec<GraphValue>> = pairs
            .into_iter()
            .map(|(a, c)| vec![GraphValue::Vertex(a), GraphValue::Vertex(c)])
            .collect();

        let text = "MATCH (a:Person)-[:KNOWS]->()-[:KNOWS]->(c) RETURN a,c ORDER BY a,c";
        let result = db
            .query(
                &cx,
                text,
                &GqlParameters::new(),
                symbols,
                GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
            )
            .unwrap_or_else(|error| panic!("query={text}: {error:?}"));
        let QueryResult::Rows { rows, .. } = result else {
            panic!("expected rows for {text}, got {result:?}");
        };
        let actual: Vec<Vec<GraphValue>> = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| match cell {
                        GraphAggregateValue::Value(value) => value,
                        other => panic!("expected vertex value cell, got {other:?}"),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(actual, expected, "query={text}");
    });
    assert!(report.lab_test_passed(), "report={report:?}");
}

/// Plain bounded MATCH is a WALK: parallel edges, revisited vertices and
/// reused edges each contribute their full occurrence count. The reference
/// expands only public storage scans, without invoking any GQL evaluator.
#[test]
fn bounded_plain_match_walks_match_storage_scan_oracle() {
    for seed in [0xD011_u64, 0xD012, 0xD013] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let dir = std::env::temp_dir().join(format!(
                "fgdb-oracle-dbcg-bounded-{}-{seed}",
                std::process::id()
            ));
            let keys = DatabaseKeys::new(
                [0x6b; 32],
                DatabaseSecurityNamespaceId([0x78; 32]),
                [0x3d; 32],
            );
            let mut db = Database::create(&commit, &dir, keys)
                .await
                .expect("bounded oracle database");
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(1), vec![PERSON], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.create_vertex(VId(3), vec![PERSON], vec![]);
            batch.add_edge(EId(1), VId(1), VId(2), vec![]);
            batch.add_edge(EId(2), VId(1), VId(2), vec![]);
            batch.add_edge(EId(3), VId(2), VId(1), vec![]);
            batch.add_edge(EId(4), VId(1), VId(1), vec![]);
            // VId(3) stays isolated; seeded extra parallel occurrences vary
            // the graph itself, not merely the lab scheduler.
            for extra in 0..seed % 3 {
                batch.add_edge(EId(u128::from(10 + extra)), VId(1), VId(2), vec![]);
            }
            let historical = db.write(&commit, batch).await.expect("initial graph");
            let mut deletion = WriteBatch::new(R);
            deletion.delete_edge(EId(2));
            let frontier = db
                .write(&commit, deletion)
                .await
                .expect("delete occurrence");
            let mut snapshot_outputs = Vec::new();
            for (as_of, temporal) in [
                (
                    historical,
                    format!(" FOR SYSTEM_TIME AS OF SEQ {}", historical.0),
                ),
                (frontier, String::new()),
            ] {
                let vertices = db.vertices_at(as_of).expect("visible vertices");
                let edges = db.edges_at(as_of).expect("visible edges");
                for (min, max) in [(0, 0), (0, 4), (2, 4)] {
                    let mut mode_outputs = Vec::new();
                    for mode in ["", " WALK", " TRAIL"] {
                        let mut pairs = Vec::new();
                        for source in vertices.iter().filter(|v| v.labels.contains(&PERSON)) {
                            let mut layer = vec![(source.vid, Vec::<EId>::new())];
                            for depth in 0..=max {
                                if depth >= min {
                                    pairs.extend(
                                        layer.iter().map(|(target, _)| (source.vid, *target)),
                                    );
                                }
                                if depth == max {
                                    break;
                                }
                                let mut next = Vec::new();
                                for (at, used) in layer {
                                    for edge in edges.iter().filter(|edge| {
                                        edge.entry.relation == R && edge.entry.src == at
                                    }) {
                                        if vertices.iter().any(|v| v.vid == edge.entry.dst)
                                            && (mode != " TRAIL" || !used.contains(&edge.entry.eid))
                                        {
                                            let mut path = used.clone();
                                            path.push(edge.entry.eid);
                                            next.push((edge.entry.dst, path));
                                        }
                                    }
                                }
                                layer = next;
                            }
                        }
                        pairs.sort_unstable();
                        if max == 0 {
                            assert_eq!(pairs, vec![(VId(1), VId(1)), (VId(3), VId(3))]);
                        } else {
                            assert!(
                                pairs
                                    .iter()
                                    .filter(|pair| **pair == (VId(1), VId(1)))
                                    .count()
                                    > 1,
                                "anti-vacuity: returning cycles retain repeated occurrences"
                            );
                            if min == 0 {
                                assert!(
                                    pairs.contains(&(VId(3), VId(3))),
                                    "zero-hop isolated root"
                                );
                                if mode.is_empty() {
                                    snapshot_outputs.push(pairs.clone());
                                }
                            } else {
                                assert!(!pairs.contains(&(VId(3), VId(3))), "positive lower bound");
                            }
                        }
                        mode_outputs.push(pairs.clone());
                        let expected: Vec<Vec<GraphValue>> = pairs
                            .into_iter()
                            .map(|(a, b)| vec![GraphValue::Vertex(a), GraphValue::Vertex(b)])
                            .collect();
                        let text = format!(
                            "MATCH{mode} (a:Person)-[:KNOWS*{min}..{max}]->(b){temporal} RETURN a,b ORDER BY a,b"
                        );
                        let result = db
                            .query(
                                &cx,
                                &text,
                                &GqlParameters::new(),
                                symbols,
                                GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
                            )
                            .unwrap_or_else(|error| panic!("seed={seed} query={text}: {error:?}"));
                        let QueryResult::Rows { rows, .. } = result else {
                            panic!("expected rows for {text}, got {result:?}");
                        };
                        let actual: Vec<Vec<GraphValue>> = rows
                            .into_iter()
                            .map(|row| {
                                row.into_iter()
                                    .map(|cell| match cell {
                                        GraphAggregateValue::Value(value) => value,
                                        other => {
                                            panic!("expected vertex value cell, got {other:?}")
                                        }
                                    })
                                    .collect()
                            })
                            .collect();
                        assert_eq!(actual, expected, "seed={seed} query={text}");
                    }
                    assert_eq!(mode_outputs[0], mode_outputs[1], "plain MATCH is WALK");
                    if max > 0 {
                        assert_ne!(mode_outputs[0], mode_outputs[2], "TRAIL forbids edge reuse");
                    }
                }
            }
            assert_ne!(
                snapshot_outputs[0], snapshot_outputs[1],
                "delete changes walk multiplicity"
            );
        });
        assert!(report.lab_test_passed(), "seed={seed} report={report:?}");
    }
}

// Only canonical scalar primitives enter the reference comparator. In
// particular, neither GraphValue::cmp nor a GQL ordering helper is an oracle.
#[derive(Clone, Debug, PartialEq, Eq)]
enum HiddenOrderCell {
    Null,
    Int(i64),
    Text(String),
}

impl HiddenOrderCell {
    fn from_scalar(value: Option<&CanonicalScalar>) -> Self {
        match value {
            None | Some(CanonicalScalar::Null) => Self::Null,
            Some(CanonicalScalar::Int(value)) => Self::Int(*value),
            Some(CanonicalScalar::Text(value)) => Self::Text(value.as_str().to_owned()),
            other => panic!("unexpected fixture scalar: {other:?}"),
        }
    }

    fn scalar(&self) -> CanonicalScalar {
        match self {
            Self::Null => CanonicalScalar::Null,
            Self::Int(value) => CanonicalScalar::Int(*value),
            Self::Text(value) => CanonicalScalar::ucs_basic_text(value).unwrap(),
        }
    }

    fn compare(&self, other: &Self, descending: bool, nulls_first: bool) -> Ordering {
        let null_order = if nulls_first {
            Ordering::Less
        } else {
            Ordering::Greater
        };
        match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal,
            (Self::Null, _) => null_order,
            (_, Self::Null) => null_order.reverse(),
            (Self::Int(a), Self::Int(b)) => {
                if descending {
                    b.cmp(a)
                } else {
                    a.cmp(b)
                }
            }
            (Self::Text(a), Self::Text(b)) => {
                if descending {
                    b.as_str().cmp(a.as_str())
                } else {
                    a.as_str().cmp(b.as_str())
                }
            }
            _ => panic!("fixture compares different non-null primitive types"),
        }
    }
}

#[test]
fn hidden_order_properties_match_storage_scan_oracle() {
    for seed in [0xD021_u64, 0xD022, 0xD023] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let dir = std::env::temp_dir().join(format!(
                "fgdb-oracle-dbcg-hidden-order-{}-{seed}",
                std::process::id()
            ));
            let keys = DatabaseKeys::new(
                [0x6b; 32],
                DatabaseSecurityNamespaceId([0x78; 32]),
                [0x3d; 32],
            );
            let mut db = Database::create(&commit, &dir, keys)
                .await
                .expect("hidden ordering oracle database");
            let variant = usize::try_from(seed % 3).unwrap();
            let mut batch = WriteBatch::new(R);
            let fixture = [
                (Some("Ada"), Some(30), Some("beta")),
                (Some("Ada"), Some(30), Some("alpha")),
                (Some("Zed"), Some(30), None),
                (Some("Bea"), Some(30), None),
                (Some("Missing-age"), None, Some("beta")),
                (Some("Null-age"), None, Some("alpha")),
                (None, Some(-2), Some("alpha")),
                (None, Some(30), Some("alpha")),
                (
                    Some(["Iris", "Mara", "Zoe"][variant]),
                    Some(70 + variant as i64),
                    Some("gamma"),
                ),
            ];
            for (index, (name, age, city)) in fixture.into_iter().enumerate() {
                let id = VId(index as u128 + 1);
                let mut props = Vec::new();
                // The paired absences and explicit NULLs must be read from
                // storage, not inferred from fixture indices by the oracle.
                if id != VId(7) {
                    props.push((
                        NAME,
                        name.map_or(CanonicalScalar::Null, |value| {
                            CanonicalScalar::ucs_basic_text(value).unwrap()
                        }),
                    ));
                }
                if id != VId(5) {
                    props.push((AGE, age.map_or(CanonicalScalar::Null, CanonicalScalar::Int)));
                }
                if id != VId(3) {
                    props.push((
                        CITY,
                        city.map_or(CanonicalScalar::Null, |value| {
                            CanonicalScalar::ucs_basic_text(value).unwrap()
                        }),
                    ));
                }
                batch.create_vertex(id, vec![PERSON], props);
            }
            batch.create_vertex(VId(100), vec![], vec![]);
            for id in 1..=9 {
                batch.add_edge(EId(id), VId(id), VId(100), vec![]);
            }
            // The seeds vary graph topology as well as scalar data. Parallel
            // edges produce duplicate visible rows that sorting must retain.
            for extra in 0..=variant {
                batch.add_edge(EId(100 + extra as u128), VId(2), VId(100), vec![]);
            }
            let historical = db
                .write(&commit, batch)
                .await
                .expect("initial ordering graph");
            let mut mutation = WriteBatch::new(R);
            mutation.set_vertex_property(VId(1), AGE, Some(CanonicalScalar::Int(-10)));
            mutation.set_vertex_property(VId(1), CITY, Some(CanonicalScalar::Null));
            mutation.delete_edge(EId(1));
            mutation.delete_vertex(VId(9));
            let frontier = db
                .write(&commit, mutation)
                .await
                .expect("mutate and delete ordering graph");

            let mut snapshot_outputs = Vec::new();
            for (as_of, temporal) in [
                (
                    historical,
                    format!(" FOR SYSTEM_TIME AS OF SEQ {}", historical.0),
                ),
                (frontier, String::new()),
                (
                    frontier,
                    format!(" FOR SYSTEM_TIME AS OF SEQ {}", frontier.0),
                ),
            ] {
                let vertices = db.vertices_at(as_of).expect("ordering vertex scan");
                let edges = db.edges_at(as_of).expect("ordering edge scan");
                let person_rows: Vec<_> = vertices
                    .iter()
                    .filter(|vertex| vertex.labels.contains(&PERSON))
                    .map(|vertex| {
                        let cells = [NAME, AGE, CITY].map(|key| {
                            HiddenOrderCell::from_scalar(
                                vertex
                                    .props
                                    .iter()
                                    .find(|(found, _)| *found == key)
                                    .map(|(_, value)| value),
                            )
                        });
                        (vertex.vid, cells)
                    })
                    .collect();
                assert_eq!(person_rows.len(), if as_of == historical { 9 } else { 8 });
                for (missing, explicit, key) in [
                    (VId(5), VId(6), AGE),
                    (VId(7), VId(8), NAME),
                    (VId(3), VId(4), CITY),
                ] {
                    let absent = vertices.iter().find(|v| v.vid == missing).unwrap();
                    let null = vertices.iter().find(|v| v.vid == explicit).unwrap();
                    assert!(!absent.props.iter().any(|(found, _)| *found == key));
                    assert!(
                        null.props
                            .iter()
                            .any(|(found, value)| *found == key && *value == CanonicalScalar::Null)
                    );
                }
                for edge_pattern in [false, true] {
                    let input: Vec<[HiddenOrderCell; 3]> = if edge_pattern {
                        edges
                            .iter()
                            .filter(|edge| edge.entry.relation == R)
                            .filter(|edge| vertices.iter().any(|v| v.vid == edge.entry.dst))
                            .filter_map(|edge| {
                                person_rows.iter().find(|(vid, _)| *vid == edge.entry.src)
                            })
                            .map(|(_, cells)| cells.clone())
                            .collect()
                    } else {
                        person_rows.iter().map(|(_, cells)| cells.clone()).collect()
                    };
                    assert_eq!(
                        input
                            .iter()
                            .filter(|cells| cells[0] == HiddenOrderCell::Text("Ada".to_owned()))
                            .count(),
                        if edge_pattern {
                            2 + variant + usize::from(as_of == historical)
                        } else {
                            2
                        },
                        "anti-vacuity: duplicate projected values survive mutation and edge bags"
                    );
                    assert!(
                        input
                            .iter()
                            .any(|a| input.iter().any(|b| a[1] == b[1] && a[0] != b[0])),
                        "anti-vacuity: hidden ties require the visible tuple tie-break"
                    );

                    // Key tuples are (primitive cell index, descending, nulls first).
                    let mut orders = vec![("p.age DESC".to_owned(), vec![(1, true, false)])];
                    for descending in [false, true] {
                        for first in [false, true] {
                            let direction = if descending { "DESC" } else { "ASC" };
                            let nulls = if first { "FIRST" } else { "LAST" };
                            for (property, index) in [("age", 1), ("city", 2)] {
                                orders.push((
                                    format!("p.{property} {direction} NULLS {nulls}"),
                                    vec![(index, descending, first)],
                                ));
                            }
                            orders.push((
                                format!("p.age {direction} NULLS {nulls},p.city DESC NULLS FIRST"),
                                vec![(1, descending, first), (2, true, true)],
                            ));
                        }
                    }
                    for (order_index, (order, keys)) in orders.iter().enumerate() {
                        let mut ranked = input.clone();
                        ranked.sort_by(|a, b| {
                            for &(index, descending, first) in keys {
                                let compared = a[index].compare(&b[index], descending, first);
                                if compared != Ordering::Equal {
                                    return compared;
                                }
                            }
                            // The public tuple is (n), independently ordered
                            // ascending with the canonical NULL-before-text tie-break.
                            a[0].compare(&b[0], false, true)
                        });
                        if !edge_pattern && order_index == 0 {
                            snapshot_outputs
                                .push(ranked.iter().map(|row| row[0].clone()).collect::<Vec<_>>());
                        }
                        for (skip, limit) in [
                            (0, None),
                            (0, Some(2)),
                            (1, Some(3)),
                            (2, Some(0)),
                            (100, Some(2)),
                        ] {
                            let page = match (skip, limit) {
                                (0, None) => String::new(),
                                (0, Some(limit)) => format!(" LIMIT {limit}"),
                                (skip, Some(limit)) => format!(" SKIP {skip} LIMIT {limit}"),
                                (skip, None) => format!(" SKIP {skip}"),
                            };
                            let pattern = if edge_pattern {
                                "(p:Person)-[:KNOWS]->(q)"
                            } else {
                                "(p:Person)"
                            };
                            let text = format!(
                                "MATCH {pattern}{temporal} RETURN p.name AS n ORDER BY {order}{page}"
                            );
                            let expected: Vec<Vec<GraphValue>> = ranked
                                .iter()
                                .skip(skip)
                                .take(limit.unwrap_or(usize::MAX))
                                .map(|row| vec![GraphValue::Scalar(row[0].scalar())])
                                .collect();
                            let result = db
                                .query(
                                    &cx,
                                    &text,
                                    &GqlParameters::new(),
                                    symbols,
                                    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
                                )
                                .unwrap_or_else(|error| {
                                    panic!("seed={seed} query={text}: {error:?}")
                                });
                            let QueryResult::Rows { columns, rows } = result else {
                                panic!("expected rows for {text}, got {result:?}");
                            };
                            assert_eq!(columns, vec!["n".to_owned()], "seed={seed} query={text}");
                            let actual: Vec<Vec<GraphValue>> = rows
                                .into_iter()
                                .map(|row| {
                                    assert_eq!(
                                        row.len(),
                                        1,
                                        "hidden keys leaked: seed={seed} query={text}"
                                    );
                                    row.into_iter()
                                        .map(|cell| match cell {
                                            GraphAggregateValue::Value(value) => value,
                                            other => panic!("expected scalar cell, got {other:?}"),
                                        })
                                        .collect()
                                })
                                .collect();
                            assert_eq!(actual, expected, "seed={seed} query={text}");
                        }
                    }
                }
            }
            assert_ne!(
                snapshot_outputs[0], snapshot_outputs[1],
                "mutation/deletion changes visible ranking"
            );
            assert_eq!(
                snapshot_outputs[1], snapshot_outputs[2],
                "implicit current and explicit frontier agree"
            );
        });
        assert!(report.lab_test_passed(), "seed={seed} report={report:?}");
    }
}

#[test]
fn existential_subqueries_match_storage_scan_oracle() {
    for seed in [0xD031_u64, 0xD032, 0xD033] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let dir = std::env::temp_dir().join(format!(
                "fgdb-oracle-dbcg-exists-{}-{seed}",
                std::process::id()
            ));
            let mut db = Database::create(
                &commit,
                &dir,
                DatabaseKeys::new(
                    [0x6b; 32],
                    DatabaseSecurityNamespaceId([0x78; 32]),
                    [0x3d; 32],
                ),
            )
            .await
            .expect("existential oracle database");
            let mut batch = WriteBatch::new(R);
            for id in 1..=5 {
                let props = match id {
                    2 => vec![(AGE, CanonicalScalar::Int(5))],
                    3 => vec![(AGE, CanonicalScalar::Null)],
                    _ => vec![],
                };
                batch.create_vertex(VId(id), vec![PERSON], props);
            }
            batch.add_edge(EId(1), VId(1), VId(2), vec![]);
            batch.add_edge(EId(2), VId(1), VId(3), vec![]);
            batch.add_edge(EId(3), VId(2), VId(3), vec![]);
            batch.add_edge(EId(4), VId(3), VId(4), vec![]);
            for extra in 0..=seed % 3 {
                batch.add_edge(EId(u128::from(10 + extra)), VId(1), VId(2), vec![]);
            }
            let historical = db.write(&commit, batch).await.expect("existential fixture");
            let mut deletion = WriteBatch::new(R);
            deletion.delete_vertex(VId(2));
            deletion.delete_edge(EId(4));
            let frontier = db
                .write(&commit, deletion)
                .await
                .expect("existential deletion");
            let mut snapshots = Vec::new();
            for (seq, temporal) in [
                (
                    historical,
                    format!(" FOR SYSTEM_TIME AS OF SEQ {}", historical.0),
                ),
                (frontier, String::new()),
            ] {
                let vertices = db.vertices_at(seq).expect("visible vertices");
                let edges = db.edges_at(seq).expect("visible edges");
                let mut cases = Vec::new();
                for filtered in [false, true] {
                    let mut partition = Vec::new();
                    for anti in [false, true] {
                        let mut expected: Vec<VId> = vertices
                            .iter()
                            .filter(|v| v.labels.contains(&PERSON))
                            .filter(|source| {
                                let exists = edges.iter().any(|edge| {
                                    edge.entry.relation == R
                                        && edge.entry.src == source.vid
                                        && vertices.iter().any(|target| {
                                            target.vid == edge.entry.dst
                                                && (!filtered
                                                    || target.props.iter().any(|(key, value)| {
                                                        *key == AGE
                                                            && matches!(
                                                                value,
                                                                CanonicalScalar::Int(n) if *n > 0
                                                            )
                                                    }))
                                        })
                                });
                                exists != anti
                            })
                            .map(|v| v.vid)
                            .collect();
                        expected.sort_unstable();
                        partition.extend(expected.iter().copied());
                        let inner = if filtered {
                            "(p)-[:KNOWS]->(q) WHERE q.age > 0"
                        } else {
                            "(p)-[:KNOWS]->()"
                        };
                        let not = if anti { "NOT " } else { "" };
                        let text = format!(
                            "MATCH (p:Person) WHERE {not}EXISTS {{ MATCH {inner} }}{temporal} RETURN p ORDER BY p"
                        );
                        let result = db
                            .query(
                                &cx,
                                &text,
                                &GqlParameters::new(),
                                symbols,
                                GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
                            )
                            .unwrap_or_else(|e| panic!("seed={seed} query={text}: {e:?}"));
                        let QueryResult::Rows { rows, .. } = result else {
                            panic!("expected rows: {result:?}")
                        };
                        let actual: Vec<Vec<GraphAggregateValue>> = rows.into_iter().collect();
                        let expected_rows: Vec<Vec<GraphAggregateValue>> = expected
                            .iter()
                            .map(|vid| vec![GraphAggregateValue::Value(GraphValue::Vertex(*vid))])
                            .collect();
                        assert_eq!(actual, expected_rows, "seed={seed} query={text}");
                        cases.push(expected);
                    }
                    partition.sort_unstable();
                    let mut all: Vec<_> = vertices
                        .iter()
                        .filter(|v| v.labels.contains(&PERSON))
                        .map(|v| v.vid)
                        .collect();
                    all.sort_unstable();
                    assert_eq!(
                        partition, all,
                        "semi/anti partition, never witness multiplicity"
                    );
                }
                assert!(!cases[0].is_empty() && !cases[1].is_empty());
                assert!(
                    cases[1].contains(&VId(5)),
                    "isolated vertex is an anti match"
                );
                assert_ne!(
                    cases[0], cases[2],
                    "NULL and missing property witnesses differ from positive integers"
                );
                snapshots.push(cases);
            }
            assert_ne!(
                snapshots[0], snapshots[1],
                "deleted witnesses change existence"
            );
        });
        assert!(report.lab_test_passed(), "seed={seed} report={report:?}");
    }
}
