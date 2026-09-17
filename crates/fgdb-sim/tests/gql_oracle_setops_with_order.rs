//! Independent relational oracle: count arithmetic rather than engine set merging.
//! Ordering follows plan §8.6: explicit keys, then canonical complete-row LexMin.
use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSetText,
    PreparedTemporalGraphSetText,
};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::{replay, replay_through};
use fgdb_types::context::{PurposeContexts, QueryCx};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, GraphId, VId};
use std::collections::BTreeMap;

const K: PropertyKeyId = PropertyKeyId(1);
const T: PropertyKeyId = PropertyKeyId(2);
type Row = Vec<CanonicalScalar>;

fn project(graph: &ReferenceGraph) -> Vec<Row> {
    graph
        .iter_vertices()
        .map(|(_, v)| {
            [K, T]
                .into_iter()
                .map(|key| v.props.get(&key).cloned().unwrap_or(CanonicalScalar::Null))
                .collect()
        })
        .collect()
}

fn union(left: &[Row], right: &[Row], all: bool) -> Vec<Row> {
    let mut counts = BTreeMap::<Row, usize>::new();
    for row in left.iter().chain(right) {
        *counts.entry(row.clone()).or_default() += 1;
    }
    counts
        .into_iter()
        .flat_map(|(row, n)| std::iter::repeat_n(row, if all { n } else { 1 }))
        .collect()
}

#[derive(Clone, Copy)]
enum Operation {
    Union,
    Intersect,
    Except,
}

fn set(left: &[Row], right: &[Row], op: Operation, all: bool) -> Vec<Row> {
    if matches!(op, Operation::Union) {
        return union(left, right, all);
    }
    let mut counts = BTreeMap::<Row, (usize, usize)>::new();
    for row in left {
        counts.entry(row.clone()).or_default().0 += 1;
    }
    for row in right {
        counts.entry(row.clone()).or_default().1 += 1;
    }
    counts
        .into_iter()
        .flat_map(|(row, (a, b))| {
            let n = match (op, all) {
                (Operation::Intersect, false) => usize::from(a > 0 && b > 0),
                (Operation::Intersect, true) => a.min(b),
                (Operation::Except, false) => usize::from(a > 0 && b == 0),
                (Operation::Except, true) => a.saturating_sub(b),
                (Operation::Union, _) => a + b,
            };
            std::iter::repeat_n(row, n)
        })
        .collect()
}

fn page(mut rows: Vec<Row>, keys: &[(usize, bool, bool)], skip: usize, limit: usize) -> Vec<Row> {
    rows.sort_by(|a, b| {
        for &(column, descending, nulls_first) in keys {
            let an = a[column] == CanonicalScalar::Null;
            let bn = b[column] == CanonicalScalar::Null;
            let order = if an != bn {
                if an == nulls_first {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            } else if descending {
                b[column].cmp(&a[column])
            } else {
                a[column].cmp(&b[column])
            };
            if !order.is_eq() {
                return order;
            }
        }
        a.cmp(b)
    });
    rows.into_iter().skip(skip).take(limit).collect()
}

fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "k") => Some(GraphSymbol::Property(K)),
        (GraphSymbolKind::Property, "t") => Some(GraphSymbol::Property(T)),
        _ => None,
    }
}

fn execute(db: &Database, cx: &QueryCx, text: &str, params: &GqlParameters) -> Vec<Row> {
    let policy = GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000);
    let rows = if text.contains("FOR SYSTEM_TIME") {
        let bound = PreparedTemporalGraphSetText::prepare(text, resolve)
            .expect("temporal prepares")
            .bind_parameters(params)
            .expect("temporal binds");
        db.execute_temporal_graph_set_text_governed(cx, &bound, policy)
            .expect("temporal executes")
            .value
    } else {
        let bound = PreparedGraphSetText::prepare(text, resolve)
            .expect("set text prepares")
            .bind_parameters(params)
            .expect("parameters bind");
        db.execute_graph_set_governed(cx, &bound, policy)
            .expect("query executes")
            .value
    };
    rows.iter()
        .map(|row| {
            row.values()
                .iter()
                .map(|cell| cell.as_scalar().expect("scalar").clone())
                .collect()
        })
        .collect()
}

fn compare_families(
    db: &Database,
    cx: &QueryCx,
    graph: &ReferenceGraph,
    seed: u64,
    temporal: &str,
) {
    let base = project(graph);
    let empty = GqlParameters::new();
    let leaf = format!("MATCH (n) {temporal} RETURN ALL n.k AS k, n.t AS t");
    let right_text = "MATCH (n) WHERE n.k >= 1 RETURN ALL n.k AS k, n.t AS t";
    let right: Vec<_> = base
        .iter()
        .filter(|r| matches!(r[0], CanonicalScalar::Int(n) if n >= 1))
        .cloned()
        .collect();
    assert!(base.iter().any(|r| r[0] == CanonicalScalar::Null));
    assert!(base.iter().any(|r| r[1] == CanonicalScalar::Null));
    assert!(
        base.iter()
            .any(|a| base.iter().any(|b| a[0] == b[0] && a != b)),
        "distinct rows must tie on integer key"
    );
    assert!(
        union(&base, &[], false).len() < base.len(),
        "input duplicates required"
    );
    assert_ne!(union(&base, &right, false), union(&base, &right, true));
    for (name, op, all) in [
        ("UNION", Operation::Union, false),
        ("UNION ALL", Operation::Union, true),
        ("INTERSECT", Operation::Intersect, false),
        ("INTERSECT ALL", Operation::Intersect, true),
        ("EXCEPT", Operation::Except, false),
        ("EXCEPT ALL", Operation::Except, true),
    ] {
        let text = format!("{leaf} {name} {right_text}");
        assert_eq!(
            execute(db, cx, &text, &empty),
            set(&base, &right, op, all),
            "seed={seed} query={text}"
        );
    }
    let unique_right = union(&right, &[], false);
    let difference = set(&base, &unique_right, Operation::Except, true);
    assert!(
        difference.iter().any(|r| unique_right.contains(r)),
        "EXCEPT ALL must retain excess left occurrences"
    );
    let text =
        format!("{leaf} EXCEPT ALL MATCH (n) WHERE n.k >= 1 RETURN DISTINCT n.k AS k, n.t AS t");
    assert_eq!(
        execute(db, cx, &text, &empty),
        difference,
        "seed={seed} query={text}"
    );
    for column in 0..2 {
        for descending in [false, true] {
            for nulls_first in [false, true] {
                let key = if column == 0 { "k" } else { "t" };
                let direction = if descending { "DESC" } else { "ASC" };
                let nulls = if nulls_first { "FIRST" } else { "LAST" };
                let ordered = format!("{leaf} ORDER BY {key} {direction} NULLS {nulls}");
                for (skip, limit) in [(0, 100), (1, 5), (100, 3), (0, 0)] {
                    let expected = page(
                        base.clone(),
                        &[(column, descending, nulls_first)],
                        skip,
                        limit,
                    );
                    let text = format!("{ordered} SKIP {skip} LIMIT {limit}");
                    assert_eq!(
                        execute(db, cx, &text, &empty),
                        expected,
                        "seed={seed} query={text}"
                    );
                    let mut params = GqlParameters::new();
                    params
                        .insert("offset", fgdb_gql::GqlParameterValue::UInt64(skip as u64))
                        .expect("offset");
                    params
                        .insert("count", fgdb_gql::GqlParameterValue::UInt64(limit as u64))
                        .expect("count");
                    let text = format!("{ordered} SKIP $offset LIMIT $count");
                    assert_eq!(
                        execute(db, cx, &text, &params),
                        expected,
                        "seed={seed} query={text}"
                    );
                }
            }
        }
    }
    // WITH's documented stage contract is projection -> page -> WHERE;
    // a subsequent WITH allows filtering before the next ordered page.
    if temporal.is_empty() {
        let filtered: Vec<_> = base
            .iter()
            .filter(|r| matches!(r[0], CanonicalScalar::Int(n) if n >= 1))
            .cloned()
            .collect();
        let selected = page(filtered, &[(1, true, true)], 1, 5);
        let text = "MATCH (n) WITH n.k AS k, n.t AS t WHERE k >= 1 WITH k, t ORDER BY t DESC NULLS FIRST SKIP 1 LIMIT 5 RETURN k, t ORDER BY k ASC NULLS LAST";
        assert_eq!(
            execute(db, cx, text, &empty),
            page(selected, &[(0, false, false)], 0, 100),
            "seed={seed} query={text}"
        );
        let selected = page(base.clone(), &[(0, true, false)], 0, 5);
        let selected = selected
            .into_iter()
            .filter(|r| r[1] != CanonicalScalar::Null)
            .collect();
        let text = "MATCH (n) WITH n.k AS k, n.t AS t ORDER BY k DESC NULLS LAST LIMIT 5 WHERE t IS NOT NULL RETURN k, t ORDER BY t ASC NULLS FIRST";
        assert_eq!(
            execute(db, cx, text, &empty),
            page(selected, &[(1, false, true)], 0, 100),
            "seed={seed} query={text}"
        );
    }
}

#[test]
fn seeded_setops_with_order_match_independent_reference() {
    for seed in [0x7311_u64, 0x7312, 0x7313, 0x7314] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).commit();
            let query_cx = PurposeContexts::narrow_runtime_root(&root).query();
            let dir = std::env::temp_dir()
                .join(format!("fgdb-setops-oracle-{}-{seed}", std::process::id()));
            let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
            let mut db = Database::create(
                &cx,
                &dir,
                DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
            )
            .await
            .expect("create");
            let mut batch = WriteBatch::new(RelationId(1));
            let mut state = seed;
            for id in 1..=16_u128 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let k = if id % 4 == 0 {
                    CanonicalScalar::Null
                } else {
                    CanonicalScalar::Int(((state >> 32) % 3) as i64)
                };
                let text = if id % 3 == 0 {
                    CanonicalScalar::Null
                } else {
                    CanonicalScalar::ucs_basic_text(if state & 1 == 0 { "a" } else { "z" })
                        .expect("text")
                };
                batch.create_vertex(VId(id), vec![], vec![(K, k), (T, text)]);
            }
            db.write(&cx, batch).await.expect("seed commits");
            let earlier = db.frontier().expect("earlier frontier");
            let mut deletes = WriteBatch::new(RelationId(1));
            deletes.delete_vertex(VId(1));
            deletes.delete_vertex(VId(2));
            db.write(&cx, deletes).await.expect("deletes commit");
            let keys = CapsuleKeys::new(
                [0x5a; 32],
                namespace,
                [0x3c; 32],
                CAPSULE_OBJECT_KIND,
                CapsuleProfile::balanced(),
            );
            let coordinator = CommitCoordinator::open(&cx, &dir, keys)
                .await
                .expect("oracle opens");
            let reference = replay(&cx, &coordinator)
                .await
                .expect("oracle replay")
                .database;
            let graph = reference.graph(GraphId(1), BranchId(1)).expect("graph");
            assert_eq!(graph.iter_vertices().count(), 14);
            compare_families(&db, &query_cx, graph, seed, "");
            let prefix = replay_through(&cx, &coordinator, earlier)
                .await
                .expect("prefix replays")
                .database;
            let old_graph = prefix.graph(GraphId(1), BranchId(1)).expect("old graph");
            assert_eq!(old_graph.iter_vertices().count(), 16);
            assert_ne!(project(old_graph), project(graph));
            compare_families(
                &db,
                &query_cx,
                old_graph,
                seed,
                &format!("FOR SYSTEM_TIME AS OF SEQ {}", earlier.0),
            );
        });
        assert!(report.lab_test_passed(), "seed={seed} report={report:?}");
    }
}
