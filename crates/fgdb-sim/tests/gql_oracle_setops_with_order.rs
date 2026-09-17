//! Independent relational oracle: count arithmetic rather than engine set merging.
//! Ordering follows plan §8.6: explicit keys, then canonical complete-row LexMin.
//!
//! Pass one executes every generated statement through the real text facades
//! while the writer is open. Pass two drops the writer, replays the durable
//! stream into `fgdb-reference`, and compares rows exactly — the oracle never
//! touches engine evaluation code.
use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterValue, GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphSetText, PreparedTemporalGraphSetText,
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

#[derive(Clone, Copy, Debug)]
enum Operation {
    Union,
    Intersect,
    Except,
}

/// Independent multiset semantics: ALL sums/min/subtracts occurrences and
/// DISTINCT deduplicates operands first (plan §8.3 SetOp{.., All|Distinct}).
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

/// Explicit keys, then canonical complete-row LexMin; NULLS placement never
/// reverses with DESC (set_text.rs documents the pin).
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

#[derive(Clone, Debug)]
enum Case {
    Set(Operation, bool),
    ExceptAllDistinct,
    Order {
        column: usize,
        descending: bool,
        nulls_first: bool,
        skip: usize,
        limit: usize,
    },
    With(usize),
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

fn cases(temporal: &str) -> Vec<(Case, String, GqlParameters)> {
    let mut out = Vec::new();
    let leaf = format!("MATCH (n) {temporal} RETURN ALL n.k AS k, n.t AS t");
    // The temporal selector appears exactly once (in the first head) and pins
    // EVERY operand to the same snapshot; the second arm therefore stays bare.
    let right_text = "MATCH (n) WHERE n.k >= 1 RETURN ALL n.k AS k, n.t AS t";
    for (name, op, all) in [
        ("UNION", Operation::Union, false),
        ("UNION ALL", Operation::Union, true),
        ("INTERSECT", Operation::Intersect, false),
        ("INTERSECT ALL", Operation::Intersect, true),
        ("EXCEPT", Operation::Except, false),
        ("EXCEPT ALL", Operation::Except, true),
    ] {
        out.push((
            Case::Set(op, all),
            format!("{leaf} {name} {right_text}"),
            GqlParameters::new(),
        ));
    }
    out.push((
        Case::ExceptAllDistinct,
        format!("{leaf} EXCEPT ALL MATCH (n) WHERE n.k >= 1 RETURN DISTINCT n.k AS k, n.t AS t"),
        GqlParameters::new(),
    ));
    for column in 0..2 {
        for descending in [false, true] {
            for nulls_first in [false, true] {
                let key = if column == 0 { "k" } else { "t" };
                let direction = if descending { "DESC" } else { "ASC" };
                let nulls = if nulls_first { "FIRST" } else { "LAST" };
                let ordered = format!("{leaf} ORDER BY {key} {direction} NULLS {nulls}");
                for (skip, limit) in [(0usize, 100usize), (1, 5), (100, 3), (0, 0)] {
                    out.push((
                        Case::Order {
                            column,
                            descending,
                            nulls_first,
                            skip,
                            limit,
                        },
                        format!("{ordered} SKIP {skip} LIMIT {limit}"),
                        GqlParameters::new(),
                    ));
                    let mut params = GqlParameters::new();
                    params
                        .insert("offset", GqlParameterValue::UInt64(skip as u64))
                        .expect("offset parameter");
                    params
                        .insert("count", GqlParameterValue::UInt64(limit as u64))
                        .expect("count parameter");
                    out.push((
                        Case::Order {
                            column,
                            descending,
                            nulls_first,
                            skip,
                            limit,
                        },
                        format!("{ordered} SKIP $offset LIMIT $count"),
                        params,
                    ));
                }
            }
        }
    }
    if temporal.is_empty() {
        out.push((
            Case::With(0),
            "MATCH (n) WITH n.k AS k, n.t AS t WHERE k >= 1 WITH k, t ORDER BY t DESC NULLS FIRST SKIP 1 LIMIT 5 RETURN k, t ORDER BY k ASC NULLS LAST".to_owned(),
            GqlParameters::new(),
        ));
        out.push((
            Case::With(1),
            "MATCH (n) WITH n.k AS k, n.t AS t ORDER BY k DESC NULLS LAST LIMIT 5 WHERE t IS NOT NULL RETURN k, t ORDER BY t ASC NULLS FIRST".to_owned(),
            GqlParameters::new(),
        ));
    }
    out
}

fn expected(base: &[Row], case: &Case) -> Vec<Row> {
    let right: Vec<Row> = base
        .iter()
        .filter(|r| matches!(r[0], CanonicalScalar::Int(n) if n >= 1))
        .cloned()
        .collect();
    match case {
        Case::Set(op, all) => set(base, &right, *op, *all),
        Case::ExceptAllDistinct => {
            let unique_right = union(&right, &[], false);
            set(base, &unique_right, Operation::Except, true)
        }
        Case::Order {
            column,
            descending,
            nulls_first,
            skip,
            limit,
        } => page(
            base.to_vec(),
            &[(*column, *descending, *nulls_first)],
            *skip,
            *limit,
        ),
        Case::With(0) => {
            let filtered: Vec<Row> = base
                .iter()
                .filter(|r| matches!(r[0], CanonicalScalar::Int(n) if n >= 1))
                .cloned()
                .collect();
            // Stage page precedes the (absent) following WHERE; RETURN's final
            // ORDER BY re-projects and re-orders the surviving rows.
            page(
                page(filtered, &[(1, true, true)], 1, 5),
                &[(0, false, false)],
                0,
                100,
            )
        }
        Case::With(1) => {
            let selected = page(base.to_vec(), &[(0, true, false)], 0, 5);
            let selected: Vec<Row> = selected
                .into_iter()
                .filter(|r| r[1] != CanonicalScalar::Null)
                .collect();
            page(selected, &[(1, false, true)], 0, 100)
        }
        Case::With(_) => unreachable!("two WITH cases only"),
    }
}

fn verify(base: &[Row], results: &[(Case, Vec<Row>)], seed: u64, tag: &str) {
    assert!(
        base.iter().any(|r| r[0] == CanonicalScalar::Null),
        "{tag}: NULLs required in integer sort key"
    );
    assert!(
        base.iter().any(|r| r[1] == CanonicalScalar::Null),
        "{tag}: NULLs required in text sort key"
    );
    assert!(
        base.iter()
            .any(|a| base.iter().any(|b| a[0] == b[0] && a != b)),
        "{tag}: distinct rows must tie on the integer key"
    );
    assert!(
        union(base, &[], false).len() < base.len(),
        "{tag}: input duplicates required"
    );
    let right: Vec<Row> = base
        .iter()
        .filter(|r| matches!(r[0], CanonicalScalar::Int(n) if n >= 1))
        .cloned()
        .collect();
    assert_ne!(
        union(base, &right, false),
        union(base, &right, true),
        "{tag}: UNION vs UNION ALL must be distinguishable"
    );
    for (case, rows) in results {
        assert_eq!(
            rows,
            &expected(base, case),
            "seed={seed} {tag} case={case:?}"
        );
    }
}

#[test]
fn seeded_setops_with_order_match_independent_reference() {
    for seed in [0x7311_u64, 0x7312, 0x7313, 0x7314] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let query_cx = contexts.query();
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
            // Pass one: run every generated statement while the writer lives.
            let live: Vec<_> = cases("")
                .into_iter()
                .map(|(case, text, params)| (case, execute(&db, &query_cx, &text, &params)))
                .collect();
            let selector = format!("FOR SYSTEM_TIME AS OF SEQ {}", earlier.0);
            let historical: Vec<_> = cases(&selector)
                .into_iter()
                .map(|(case, text, params)| (case, execute(&db, &query_cx, &text, &params)))
                .collect();
            drop(db);
            // Pass two: the independent oracle opens the durable stream alone.
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
            let base = project(graph);
            verify(&base, &live, seed, "live");
            let prefix = replay_through(&cx, &coordinator, earlier)
                .await
                .expect("prefix replays")
                .database;
            let old_graph = prefix.graph(GraphId(1), BranchId(1)).expect("old graph");
            assert_eq!(old_graph.iter_vertices().count(), 16);
            assert_ne!(project(old_graph), base, "delete must separate snapshots");
            verify(&project(old_graph), &historical, seed, "historical");
        });
        assert!(report.lab_test_passed(), "seed={seed} report={report:?}");
    }
}
