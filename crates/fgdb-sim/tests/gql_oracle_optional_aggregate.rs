//! Native text differentials against independently replayed ReferenceGraph state.
//! The oracle reads only reference vertices/edges and uses ordinary Rust bag,
//! grouping, and arithmetic operations: no GQL parser/evaluator/accumulator.
use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::replay;
use fgdb_types::{
    BranchId, CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, GraphId,
    PurposeContexts, QueryCx, VId,
};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const OWNER: LabelId = LabelId(1);
const TARGET: LabelId = LabelId(2);
const VALUE: PropertyKeyId = PropertyKeyId(1);
const CATEGORY: PropertyKeyId = PropertyKeyId(2);
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const OPTIONAL: &str = "MATCH (a:Owner) OPTIONAL MATCH (a)-[:R]->(b:Target)";

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Owner") => Some(GraphSymbol::Label(OWNER)),
        (GraphSymbolKind::Label, "Target") => Some(GraphSymbol::Label(TARGET)),
        (GraphSymbolKind::Property, "value") => Some(GraphSymbol::Property(VALUE)),
        (GraphSymbolKind::Property, "category") => Some(GraphSymbol::Property(CATEGORY)),
        _ => None,
    }
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x5a; 32], NAMESPACE, [0x3c; 32])
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 20_000_000, 10_000_000)
}

// A local deterministic generator; no production generator or graph semantics.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}
async fn generate(db: &mut Database<MemVfs>, cx: &CommitCx, seed: u64) -> CommitSeq {
    let mut random = seed;
    let mut batch = WriteBatch::new(R);
    for id in 1..=8 {
        batch.create_vertex(
            VId(id),
            vec![OWNER],
            vec![
                (
                    VALUE,
                    CanonicalScalar::Int(i64::try_from(next(&mut random) % 11).unwrap() - 5),
                ),
                (
                    CATEGORY,
                    CanonicalScalar::ucs_basic_text(if id % 2 == 0 { "blue" } else { "red" })
                        .unwrap(),
                ),
            ],
        );
    }
    for id in 100..108 {
        let mut props = vec![(
            CATEGORY,
            CanonicalScalar::ucs_basic_text(if next(&mut random) % 2 == 0 {
                "alpha"
            } else {
                "beta"
            })
            .unwrap(),
        )];
        // Both stored NULL and missing values; a tiny domain ensures duplicates.
        if id == 106 {
            props.push((VALUE, CanonicalScalar::Null));
        } else if id != 105 {
            props.push((
                VALUE,
                CanonicalScalar::Int(i64::try_from(next(&mut random) % 7).unwrap() - 3),
            ));
        }
        batch.create_vertex(VId(id), vec![TARGET], props);
    }
    // Duplicate edge occurrences and at least two distinct non-null values.
    batch.set_vertex_property(VId(100), VALUE, Some(CanonicalScalar::Int(-2)));
    batch.set_vertex_property(VId(101), VALUE, Some(CanonicalScalar::Int(3)));
    let mut eid = 1;
    for owner in 1..=5 {
        for target in [
            100,
            100,
            101,
            105,
            106,
            100 + u128::from(next(&mut random) % 5),
        ] {
            batch.add_edge(EId(eid), VId(owner), VId(target), vec![]);
            eid += 1;
        }
    }
    // Owner 6 becomes isolated by edge deletion; 7 is isolated from birth;
    // owner 8 and target 107 disappear through vertex/cascade deletion.
    batch.add_edge(EId(90), VId(6), VId(100), vec![]);
    batch.add_edge(EId(91), VId(8), VId(107), vec![]);
    batch.add_edge(EId(92), VId(1), VId(107), vec![]);
    db.write(cx, batch).await.unwrap();
    let mut off = WriteBatch::new(RelationId(2));
    off.add_edge(EId(93), VId(7), VId(100), vec![]);
    db.write(cx, off).await.unwrap();
    let mut deletion = WriteBatch::new(R);
    deletion.delete_edge(EId(90));
    deletion.delete_vertex(VId(8));
    deletion.delete_vertex(VId(107));
    db.write(cx, deletion).await.unwrap()
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Cell {
    Null,
    Integer(i128),
    Text(String),
    Vertex(VId),
    Mean(i128, u64),
}
type Row = Vec<Cell>;
type Scope = (VId, Option<VId>);

fn scalar(value: &CanonicalScalar) -> Cell {
    match value {
        CanonicalScalar::Null => Cell::Null,
        CanonicalScalar::Int(n) => Cell::Integer(i128::from(*n)),
        CanonicalScalar::Text(t) => Cell::Text(t.as_str().to_owned()),
        other => panic!("unexpected generated scalar: {other:?}"),
    }
}
fn property(graph: &ReferenceGraph, id: Option<VId>, key: PropertyKeyId) -> Cell {
    id.and_then(|id| graph.vertex(id))
        .and_then(|v| v.props.get(&key))
        .map_or(Cell::Null, scalar)
}
fn optional_rows(graph: &ReferenceGraph, floor: Option<i128>) -> Vec<Scope> {
    let mut rows = Vec::new();
    for (owner, _) in graph
        .iter_vertices()
        .filter(|(_, v)| v.labels.contains(&OWNER))
    {
        let before = rows.len();
        for (_, edge) in graph
            .iter_edges()
            .filter(|(_, e)| e.relation == R && e.src == owner)
        {
            let target = graph.vertex(edge.dst).unwrap();
            if target.labels.contains(&TARGET)
                && floor.is_none_or(|floor| matches!(property(graph, Some(edge.dst), VALUE), Cell::Integer(n) if n >= floor)) {
                rows.push((owner, Some(edge.dst)));
            }
        }
        if rows.len() == before {
            rows.push((owner, None));
        }
    }
    rows
}
fn engine_cell(value: &GraphValue) -> Cell {
    if let Some(id) = value.as_vertex() {
        Cell::Vertex(id)
    } else {
        scalar(value.as_scalar().expect("scalar or vertex projection"))
    }
}
fn engine_aggregate(value: &GraphAggregateValue) -> Cell {
    match value {
        GraphAggregateValue::Count(n) => Cell::Integer(i128::from(*n)),
        GraphAggregateValue::Integer(n) => Cell::Integer(*n),
        GraphAggregateValue::Value(v) => engine_cell(v),
        GraphAggregateValue::Average(v) => Cell::Mean(v.numerator(), v.denominator()),
    }
}
fn compare_pattern(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    seed: u64,
    text: &str,
    mut expected: Vec<Row>,
    ordered: bool,
) {
    let prepared = PreparedGraphText::prepare(text, symbols)
        .unwrap_or_else(|e| panic!("seed={seed:#x} {text}: {e:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let mut actual: Vec<Row> = db
        .execute_graph_pattern_governed(cx, &prepared, policy())
        .unwrap_or_else(|e| panic!("seed={seed:#x} {text}: {e:?}"))
        .value
        .iter()
        .map(|r| r.values().iter().map(engine_cell).collect())
        .collect();
    if !ordered {
        actual.sort();
        expected.sort();
    }
    assert_eq!(actual, expected, "seed={seed:#x}; query={text}");
}
fn compare_aggregate(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    seed: u64,
    text: &str,
    expected: Vec<Row>,
) {
    let prepared = PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap_or_else(|e| panic!("seed={seed:#x} {text}: {e:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let actual: Vec<Row> = db
        .execute_graph_aggregate_governed(cx, &prepared, policy())
        .unwrap_or_else(|e| panic!("seed={seed:#x} {text}: {e:?}"))
        .value
        .iter()
        .map(|r| {
            r.keys()
                .iter()
                .map(engine_cell)
                .chain(r.values().iter().map(engine_aggregate))
                .collect()
        })
        .collect();
    assert_eq!(actual, expected, "seed={seed:#x}; query={text}");
}

// SQL/GQL aggregates discard NULL arguments; COUNT(*) counts bag occurrences.
// AVG uses exact integer sum/count, independently reduced with Euclid's rule.
fn statistics(values: &[Cell], distinct: bool) -> Vec<Cell> {
    let mut present: Vec<_> = values
        .iter()
        .filter(|v| **v != Cell::Null)
        .cloned()
        .collect();
    if distinct {
        present.sort();
        present.dedup();
    }
    let count = Cell::Integer(i128::try_from(present.len()).unwrap());
    if present.is_empty() {
        return vec![count, Cell::Null, Cell::Null, Cell::Null, Cell::Null];
    }
    let numbers: Vec<_> = present
        .iter()
        .map(|v| match v {
            Cell::Integer(n) => *n,
            _ => panic!("numeric fixture"),
        })
        .collect();
    let sum: i128 = numbers.iter().sum();
    let count_n = u64::try_from(numbers.len()).unwrap();
    let (mut a, mut b) = (sum.unsigned_abs(), u128::from(count_n));
    while b != 0 {
        (a, b) = (b, a % b);
    }
    vec![
        count,
        Cell::Integer(sum),
        Cell::Integer(*numbers.iter().min().unwrap()),
        Cell::Integer(*numbers.iter().max().unwrap()),
        Cell::Mean(
            sum / i128::try_from(a).unwrap(),
            count_n / u64::try_from(a).unwrap(),
        ),
    ]
}
fn aggregate_columns() -> &'static str {
    "COUNT(*) AS n,COUNT(b) AS matched,COUNT(DISTINCT b) AS vertices,\
     COUNT(b.value) AS present,SUM(b.value) AS total,MIN(b.value) AS low,MAX(b.value) AS high,AVG(b.value) AS mean,\
     COUNT(DISTINCT b.value) AS unique_n,SUM(DISTINCT b.value) AS unique_total,MIN(DISTINCT b.value) AS unique_low,MAX(DISTINCT b.value) AS unique_high,AVG(DISTINCT b.value) AS unique_mean,\
     COUNT(b.category) AS texts,COUNT(DISTINCT b.category) AS unique_texts,MIN(b.category) AS first_text,MAX(b.category) AS last_text"
}
fn summarize(graph: &ReferenceGraph, scopes: &[Scope]) -> Row {
    let matched: Vec<_> = scopes.iter().filter_map(|(_, b)| *b).collect();
    let values: Vec<_> = scopes
        .iter()
        .map(|(_, b)| property(graph, *b, VALUE))
        .collect();
    let texts: Vec<_> = scopes
        .iter()
        .map(|(_, b)| property(graph, *b, CATEGORY))
        .filter(|v| *v != Cell::Null)
        .collect();
    let mut row = vec![
        Cell::Integer(scopes.len() as i128),
        Cell::Integer(matched.len() as i128),
        Cell::Integer(matched.into_iter().collect::<BTreeSet<_>>().len() as i128),
    ];
    row.extend(statistics(&values, false));
    row.extend(statistics(&values, true));
    row.extend([
        Cell::Integer(texts.len() as i128),
        Cell::Integer(texts.iter().collect::<BTreeSet<_>>().len() as i128),
        texts.iter().min().cloned().unwrap_or(Cell::Null),
        texts.iter().max().cloned().unwrap_or(Cell::Null),
    ]);
    row
}

fn check_families(db: &Database<MemVfs>, cx: &QueryCx, graph: &ReferenceGraph, seed: u64) {
    assert!(graph.vertex(VId(8)).is_none() && graph.vertex(VId(107)).is_none());
    assert!(graph.edge(EId(90)).is_none() && graph.edge(EId(92)).is_none());
    assert!(graph.vertex(VId(105)).unwrap().props.get(&VALUE).is_none());
    assert_eq!(
        graph.vertex(VId(106)).unwrap().props.get(&VALUE),
        Some(&CanonicalScalar::Null)
    );
    for floor in [None, Some(0), Some(1000)] {
        let scopes = optional_rows(graph, floor);
        assert!(
            scopes.iter().any(|(_, b)| b.is_none()),
            "optional anti-vacuity seed={seed:#x}"
        );
        if floor == Some(1000) {
            assert!(scopes.iter().all(|(_, b)| b.is_none()));
        } else {
            assert!(scopes.iter().any(|(_, b)| b.is_some()));
        }
        let head = floor.map_or_else(
            || OPTIONAL.to_owned(),
            |n| format!("{OPTIONAL} WHERE b.value >= {n}"),
        );
        let projected: Vec<Row> = scopes
            .iter()
            .map(|(a, b)| {
                vec![
                    Cell::Vertex(*a),
                    b.map_or(Cell::Null, Cell::Vertex),
                    property(graph, *b, VALUE),
                    property(graph, *b, CATEGORY),
                ]
            })
            .collect();
        compare_pattern(
            db,
            cx,
            seed,
            &format!("{head} RETURN ALL a,b,b.value,b.category"),
            projected,
            false,
        );
        let mut distinct: Vec<Row> = scopes
            .iter()
            .map(|(a, b)| vec![Cell::Vertex(*a), property(graph, *b, VALUE)])
            .collect();
        let occurrences = distinct.len();
        distinct.sort();
        distinct.dedup();
        if floor.is_none() {
            assert!(
                distinct.len() < occurrences,
                "RETURN DISTINCT must collapse duplicates"
            );
        }
        compare_pattern(
            db,
            cx,
            seed,
            &format!("{head} RETURN DISTINCT a,b.value"),
            distinct,
            false,
        );

        // Group both by element identity and by text, not engine-created groups.
        for category in [false, true] {
            let mut groups: BTreeMap<Cell, Vec<Scope>> = BTreeMap::new();
            for scope in &scopes {
                let key = if category {
                    property(graph, Some(scope.0), CATEGORY)
                } else {
                    Cell::Vertex(scope.0)
                };
                groups.entry(key).or_default().push(*scope);
            }
            if category || floor != Some(1000) {
                assert!(
                    groups.values().any(|g| g.len() > 1),
                    "multi-member groups seed={seed:#x}"
                );
            } else {
                assert!(groups.values().all(|g| g.len() == 1));
            }
            let key = if category { "a.category" } else { "a" };
            for minimum in [0, 1, 1000] {
                let mut expected: Vec<Row> = groups
                    .iter()
                    .filter(|(_, g)| g.len() > minimum)
                    .map(|(key, g)| {
                        let mut row = vec![key.clone()];
                        row.extend(summarize(graph, g));
                        row
                    })
                    .collect();
                if minimum == 1000 {
                    assert!(expected.is_empty(), "empty HAVING control");
                } else if minimum == 0 {
                    assert_eq!(expected.len(), groups.len());
                }
                expected.sort_by(|a, b| b[1].cmp(&a[1]).then(a[0].cmp(&b[0])));
                compare_aggregate(
                    db,
                    cx,
                    seed,
                    &format!(
                        "{head} RETURN {key} AS bucket,{} GROUP BY {key} HAVING n > {minimum} ORDER BY n DESC,bucket ASC",
                        aggregate_columns()
                    ),
                    expected,
                );
            }
        }
        let summary = summarize(graph, &scopes);
        assert!(
            summary[0] > summary[1],
            "COUNT(*) must include null extensions"
        );
        if floor.is_none() {
            assert!(
                summary[3] > summary[8],
                "DISTINCT numeric aggregation is non-vacuous"
            );
        }
        compare_aggregate(
            db,
            cx,
            seed,
            &format!("{head} RETURN {}", aggregate_columns()),
            vec![summary],
        );
    }
    // An empty input has one implicit global group, but no explicit groups.
    let empty = "MATCH (a:Owner) WHERE a.value > 1000 OPTIONAL MATCH (a)-[:R]->(b:Target)";
    compare_pattern(
        db,
        cx,
        seed,
        &format!("{empty} RETURN DISTINCT a,b"),
        vec![],
        false,
    );
    compare_aggregate(
        db,
        cx,
        seed,
        &format!("{empty} RETURN {}", aggregate_columns()),
        vec![summarize(graph, &[])],
    );
    compare_aggregate(
        db,
        cx,
        seed,
        &format!(
            "{empty} RETURN a,{} GROUP BY a ORDER BY n DESC,a",
            aggregate_columns()
        ),
        vec![],
    );
}

fn run_seed(seed: u64) {
    let ((), report) = run_async_under_lab(seed, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = generate(&mut db, &commit, seed).await;
        drop(db);
        let coordinator = CommitCoordinator::open_with_vfs(
            &commit,
            vfs.clone(),
            &path,
            CapsuleKeys::new(
                [0x5a; 32],
                NAMESPACE,
                [0x3c; 32],
                CAPSULE_OBJECT_KIND,
                CapsuleProfile::balanced(),
            ),
        )
        .await
        .unwrap();
        let reference = replay(&commit, &coordinator).await.unwrap().database;
        assert_eq!(
            reference.replay_frontier(),
            basis,
            "oracle materialized at engine commit"
        );
        drop(coordinator);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), basis);
        check_families(
            &db,
            &query,
            reference.graph(GraphId(1), BranchId(1)).unwrap(),
            seed,
        );
    });
    assert!(report.lab_test_passed(), "seed={seed:#x}: {report:?}");
}

#[test]
fn native_optional_aggregate_seed_ce70() {
    run_seed(0xce70);
}
#[test]
fn native_optional_aggregate_seed_a11ce() {
    run_seed(0xa11ce);
}
#[test]
fn native_optional_aggregate_seed_5eed() {
    run_seed(0x5eed);
}
#[test]
fn native_optional_aggregate_seed_d1ff() {
    run_seed(0xd1ff);
}
