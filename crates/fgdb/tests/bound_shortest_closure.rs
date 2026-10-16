//! A SHORTEST walk between one bound source and one bound target admits
//! only the edges a breadth-first search from both ends crossed before the
//! two met (fgdb-61p1n), not every edge the source reaches.
//!
//! The oracle is the same query with the target selected by a property its
//! isolated twin shares: two targets, so the plan keeps the source's
//! forward closure, and the twin, with no edge, binds no walk. Over random
//! multigraphs (self-loops, parallel edges, two relations), every selector,
//! bounds with and without zero hops, both directions and an alternation,
//! both must return the same walks, the owner's and the authorized count
//! alike. A hub fixture shows the closure: a target nine hops down a chain
//! from a hub of 2,000 leaves needs a few dozen records, not thousands.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x51; 32]);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const W: PropertyKeyId = PropertyKeyId(3);
/// Vertices 0..VERTICES carry p = q = their id; each one's twin, TWIN + id,
/// carries the same q, p = TWIN + id, and no edge.
const VERTICES: u128 = 9;
const TWIN: u128 = 100;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
        _ => None,
    }
}

fn policy(records: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(records, 100_000, 100_000_000, 10_000_000)
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(6111), NS, "graph", SchemaEpoch(0), 1).unwrap()
}

/// A grant that hides nothing.
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits {
            max_nodes: 1_000_000,
            max_work: 100_000_000,
            max_rows: 100_000,
        },
    );
    grant.labels = Scope::All;
    grant.relations = Scope::All;
    grant.properties = Scope::All;
    grant
}

fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

/// A database of `vertices`, each (id, p, q), and `edges`, each (src, dst,
/// relation), the i-th with EId and w = i + 1.
async fn database(
    cx: &CommitCx,
    vertices: &[(u128, i64, i64)],
    edges: &[(u128, u128, RelationId)],
) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x52; 32], NS, [0x53; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for &(vid, p, q) in vertices {
        batch.create_vertex(
            VId(vid),
            vec![],
            vec![(P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(q))],
        );
    }
    db.write(cx, batch).await.unwrap();
    for relation in [R, S] {
        if edges.iter().all(|&(_, _, of)| of != relation) {
            continue;
        }
        let mut batch = WriteBatch::new(relation);
        for (at, &(src, dst, of)) in edges.iter().enumerate() {
            if of == relation {
                let eid = at as u128 + 1;
                batch.add_edge(
                    EId(eid),
                    VId(src),
                    VId(dst),
                    vec![(W, CanonicalScalar::Int(eid as i64))],
                );
            }
        }
        db.write(cx, batch).await.unwrap();
    }
    db
}

/// The rows `text` returns under `records` snapshot records, in order, or
/// None when it is refused.
fn rows(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    records: u64,
) -> Option<Vec<Vec<QueryValue>>> {
    match db.query(cx, text, &GqlParameters::new(), symbols, policy(records)) {
        Ok(QueryResult::Rows { rows, .. }) => Some(rows),
        Ok(other) => panic!("{text}: not rows: {other:?}"),
        Err(_) => None,
    }
}

/// The smallest snapshot-record budget under which `text` is answered.
fn least_records(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> u64 {
    let (mut low, mut high) = (0, 1_000_000);
    assert!(
        rows(db, cx, text, high).is_some(),
        "{text} under a wide budget"
    );
    while low < high {
        let middle = low + (high - low) / 2;
        if rows(db, cx, text, middle).is_some() {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    low
}

#[test]
fn a_bound_shortest_walk_returns_what_the_forward_closure_returns() {
    let ((), report) = run_async_under_lab(0x5ec0_6111, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut state = 0x6b1d_5e7c_0000_0001_u64;
        let mut random = |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let vertices: Vec<(u128, i64, i64)> = (0..VERTICES)
            .map(|vid| (vid, vid as i64, vid as i64))
            .chain((0..VERTICES).map(|vid| (TWIN + vid, (TWIN + vid) as i64, vid as i64)))
            .collect();
        let (mut compared, mut walks, mut empty) = (0, 0, 0);
        for _ in 0..24 {
            let edges: Vec<(u128, u128, RelationId)> = (0..random(28))
                .map(|_| {
                    let relation = if random(4) == 0 { S } else { R };
                    (
                        u128::from(random(VERTICES as u64)),
                        u128::from(random(VERTICES as u64)),
                        relation,
                    )
                })
                .collect();
            let db = database(&commit, &vertices, &edges).await;
            for _ in 0..3 {
                let source = random(VERTICES as u64);
                let target = random(VERTICES as u64);
                for selector in ["ANY SHORTEST", "ALL SHORTEST"] {
                    for bounds in ["{1,2}", "{0,3}", "{1,8}", "{1,1}", "{2,5}"] {
                        for arrow in ["-[:R]->", "<-[:R]-", "-[:R|S]->"] {
                            let pattern = |key: &str| {
                                format!(
                                    "MATCH p = {selector} (a {{p: {source}}}){arrow}{bounds}\
                                     (b {{{key}: {target}}})"
                                )
                            };
                            let text = |key: &str| {
                                format!(
                                    "{} RETURN [e IN relationships(p) | e.w] AS es, \
                                     path_length(p) AS l",
                                    pattern(key)
                                )
                            };
                            let (bound, oracle) = (text("p"), text("q"));
                            let mut got = rows(&db, &cx, &bound, 1_000_000).unwrap();
                            let mut want = rows(&db, &cx, &oracle, 1_000_000).unwrap();
                            got.sort();
                            want.sort();
                            assert_eq!(got, want, "{bound}\n{edges:?}");
                            if got.is_empty() {
                                empty += 1;
                            } else {
                                walks += 1;
                            }
                            // The authorized admission's closure: the same
                            // count of walks.
                            let count = |key: &str| {
                                let query =
                                    prepare(&format!("{} RETURN COUNT(*) AS n", pattern(key)));
                                db.execute_graph_aggregate_authorized(
                                    &cx,
                                    &issuer,
                                    &token,
                                    "main",
                                    &query,
                                    policy(1_000_000),
                                    || 100,
                                )
                                .unwrap()[0]
                                    .values()
                                    .to_vec()
                            };
                            assert_eq!(count("p"), count("q"), "{bound}\n{edges:?}");
                            compared += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(compared, 24 * 3 * 2 * 5 * 3);
        assert!(walks > 300 && empty > 300, "{walks} {empty}");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A hub with 2,000 leaves; leaf 1 heads a chain of eight more vertices to
/// the target, nine hops from the hub, and vertex 9,999 has no edge.
#[test]
fn a_far_target_admits_the_edges_both_searches_crossed_not_the_hub() {
    let ((), report) = run_async_under_lab(0x5ec0_6112, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        const LEAVES: u128 = 2_000;
        const CHAIN: u128 = 5_000;
        let mut edges: Vec<(u128, u128, RelationId)> =
            (1..=LEAVES).map(|leaf| (0, leaf, R)).collect();
        edges.push((1, CHAIN + 1, R));
        for at in CHAIN + 1..CHAIN + 8 {
            edges.push((at, at + 1, R));
        }
        let vertices: Vec<(u128, i64, i64)> = (0..=LEAVES)
            .chain(CHAIN + 1..=CHAIN + 8)
            .chain([9_999])
            .map(|vid| (vid, vid as i64, vid as i64))
            .collect();
        let db = database(&commit, &vertices, &edges).await;
        let target = CHAIN + 8;
        let text = format!(
            "MATCH p = SHORTEST (a {{p: 0}})-[:R]->{{1,12}}(b {{p: {target}}}) \
             RETURN path_length(p) AS l"
        );
        let answer = rows(&db, &cx, &text, 1_000_000).unwrap();
        assert_eq!(
            answer,
            vec![vec![QueryValue::Value(
                fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(9))
            )]]
        );
        // Nine chain edges and the ten vertices they join, against the
        // 2,009 edges the source reaches.
        let least = least_records(&db, &cx, &text);
        assert!(least <= 40, "least {least}");
        // Within eight hops there is no walk: no rows, and the search stops
        // at the bound.
        let short = text.replace("{1,12}", "{1,8}");
        assert_eq!(rows(&db, &cx, &short, 1_000_000), Some(vec![]));
        assert!(least_records(&db, &cx, &short) <= 40);
        // A target with no edge: the backward search ends at once.
        let none = text.replace(&format!("p: {target}"), "p: 9999");
        assert_eq!(rows(&db, &cx, &none, 1_000_000), Some(vec![]));
        assert!(least_records(&db, &cx, &none) <= 40);
        // The reverse direction searches the same edges from the other end.
        let reverse = format!(
            "MATCH p = ALL SHORTEST (a {{p: {target}}})<-[:R]-{{1,12}}(b {{p: 0}}) \
             RETURN path_length(p) AS l"
        );
        assert_eq!(rows(&db, &cx, &reverse, 1_000_000), Some(answer));
        assert!(least_records(&db, &cx, &reverse) <= 40);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
