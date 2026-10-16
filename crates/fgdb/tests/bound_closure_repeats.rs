//! An authorized bound chain whose expansions share one relation and one
//! direction admits each closure edge once even when a later step expands a
//! vertex an earlier one expanded (fgdb-61p1n): such a closure keeps no edge
//! identity set, so a vertex met again only records far ends.
//!
//! The oracle is the same chain with its last step widened to `E|G`, where no
//! G edge exists: the same edges, through the closure that keeps identities.
//! Both must give the brute-force trail count and need the same least
//! snapshot-record budget; an edge admitted twice would charge another
//! record (and fail the closure's debug check). A chain that changes
//! relation keeps identities, and must still admit the second relation's
//! edges of a vertex the first expansion met.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x61; 32]);
const E: RelationId = RelationId(1);
const F: RelationId = RelationId(2);
const G: RelationId = RelationId(3);
const P: PropertyKeyId = PropertyKeyId(1);

/// A root self-loop, parallel edges, cycles back to the root and through
/// its neighbours, a tail, and F edges from vertices E chains expand again.
const EDGES: [(u128, u128, RelationId); 18] = [
    (0, 0, E),
    (0, 1, E),
    (0, 1, E),
    (0, 2, E),
    (1, 0, E),
    (1, 3, E),
    (2, 1, E),
    (2, 2, E),
    (2, 0, E),
    (3, 0, E),
    (3, 4, E),
    (4, 5, E),
    (5, 3, E),
    (4, 0, E),
    (0, 3, F),
    (0, 5, F),
    (2, 4, F),
    (0, 0, F),
];
const VERTICES: u128 = 6;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "E") => Some(GraphSymbol::Relation(E)),
        (GraphSymbolKind::Relation, "F") => Some(GraphSymbol::Relation(F)),
        (GraphSymbolKind::Relation, "G") => Some(GraphSymbol::Relation(G)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn policy(records: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(records, 1_000, 100_000_000, 10_000_000)
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(6101), NS, "graph", SchemaEpoch(0), 1).unwrap()
}

/// A grant that hides nothing.
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits {
            max_nodes: 1_000_000,
            max_work: 100_000_000,
            max_rows: 1000,
        },
    );
    grant.labels = Scope::All;
    grant.relations = Scope::All;
    grant.properties = Scope::All;
    grant
}

async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x62; 32], NS, [0x63; 32]))
        .await
        .unwrap();
    let mut vertices = WriteBatch::new(E);
    for vid in 0..VERTICES {
        vertices.create_vertex(
            VId(vid),
            vec![],
            vec![(P, CanonicalScalar::Int(vid as i64))],
        );
    }
    db.write(cx, vertices).await.unwrap();
    for relation in [E, F] {
        let mut batch = WriteBatch::new(relation);
        for (at, &(src, dst, of)) in EDGES.iter().enumerate() {
            if of == relation {
                batch.add_edge(EId(at as u128 + 1), VId(src), VId(dst), vec![]);
            }
        }
        db.write(cx, batch).await.unwrap();
    }
    db
}

fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

/// Trails from vertex 0 whose i-th edge has relation `relations[i]`, all
/// forward or all reverse: walks that never reuse an edge.
fn trails(relations: &[RelationId], forward: bool) -> u64 {
    fn extend(at: u128, relations: &[RelationId], forward: bool, used: &mut Vec<usize>) -> u64 {
        let Some((&relation, rest)) = relations.split_first() else {
            return 1;
        };
        let mut total = 0;
        for (edge, &(src, dst, of)) in EDGES.iter().enumerate() {
            let (near, far) = if forward { (src, dst) } else { (dst, src) };
            if of == relation && near == at && !used.contains(&edge) {
                used.push(edge);
                total += extend(far, rest, forward, used);
                used.pop();
            }
        }
        total
    }
    extend(0, relations, forward, &mut Vec::new())
}

#[test]
fn uniform_chains_admit_each_edge_once_when_a_step_expands_a_vertex_again() {
    let ((), report) = run_async_under_lab(0x5ec0_6101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let db = database(&commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let answer = |query: &PreparedGraphAggregate, records| {
            db.execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                query,
                policy(records),
                || 100,
            )
            .ok()
            .map(|rows| rows[0].values().to_vec())
        };
        // The smallest snapshot-record budget a query is admitted under.
        let least = |query: &PreparedGraphAggregate| {
            let (mut low, mut high) = (0, 10_000);
            assert!(answer(query, high).is_some());
            while low < high {
                let middle = low + (high - low) / 2;
                if answer(query, middle).is_some() {
                    high = middle;
                } else {
                    low = middle + 1;
                }
            }
            low
        };
        let counted = |relations: &[RelationId], forward| {
            let expected = trails(relations, forward);
            assert!(expected > 0, "{relations:?} {forward}");
            Some(vec![GraphAggregateValue::Count(expected)])
        };
        let arrow = |name: &str, forward: bool| {
            if forward {
                format!("-[:{name}]->")
            } else {
                format!("<-[:{name}]-")
            }
        };
        let mut checked = 0;
        for (hops, forward) in [(2, true), (3, true), (2, false), (3, false)] {
            let step = arrow("E", forward);
            let mut chain = String::from("(a {p: 0})");
            for at in 1..hops {
                chain.push_str(&format!("{step}(v{at})"));
            }
            let uniform = prepare(&format!("MATCH {chain}{step}(z) RETURN COUNT(*) AS n"));
            let widened = arrow("E|G", forward);
            let oracle = prepare(&format!("MATCH {chain}{widened}(z) RETURN COUNT(*) AS n"));
            let walks = counted(&vec![E; hops], forward);
            assert_eq!(answer(&uniform, 10_000), walks, "{hops} {forward}");
            assert_eq!(answer(&oracle, 10_000), walks, "{hops} {forward}");
            assert_eq!(least(&uniform), least(&oracle), "{hops} {forward}");
            // Changing relation for the last step: the first expansion met
            // vertices (the root, through its self-loop) whose F edges only
            // the last step admits.
            let mixed = arrow("F", forward);
            let mixed = prepare(&format!("MATCH {chain}{mixed}(z) RETURN COUNT(*) AS n"));
            let mut relations = vec![E; hops - 1];
            relations.push(F);
            if trails(&relations, forward) > 0 {
                assert_eq!(
                    answer(&mixed, 10_000),
                    counted(&relations, forward),
                    "{hops} {forward} mixed"
                );
                checked += 1;
            }
            checked += 1;
        }
        assert!(checked >= 6, "{checked}");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Closure shapes whose admission charges a refactor of the closure must
/// keep, as least budgets per dimension (snapshot records, evaluator work,
/// evaluator scratch, the grant's signed work and nodes), recorded at
/// 13e2c5fa (fgdb-61p1n): an edge-rooted uniform chain, walks with and
/// without zero hops, an undirected chain, and chains that change relation
/// or direction.
const PINNED: [(&str, [u64; 5]); 6] = [
    (
        "MATCH (a {p: 0})-[:E]->(b)-[:E]->(c) RETURN COUNT(*) AS n",
        [13, 101, 37, 109, 4],
    ),
    (
        "MATCH (a {p: 0})-[:E]->{1,3}(b) RETURN COUNT(DISTINCT b) AS n",
        [16, 121, 46, 130, 5],
    ),
    (
        "MATCH (a {p: 0})-[:E]->{0,2}(b) RETURN COUNT(DISTINCT b) AS n",
        [13, 99, 38, 107, 4],
    ),
    (
        "MATCH (a {p: 0})-[:E]-(b)-[:E]-(c) RETURN COUNT(*) AS n",
        [20, 333, 64, 343, 6],
    ),
    (
        "MATCH (a {p: 0})-[:E]->(b)-[:F]->(c) RETURN COUNT(*) AS n",
        [14, 98, 37, 108, 6],
    ),
    (
        "MATCH (a {p: 0})<-[:E]-(b)-[:E]->(c) RETURN COUNT(*) AS n",
        [19, 268, 67, 278, 6],
    ),
];

/// A grant that hides nothing, with these signed-work and node limits.
fn limited(max_work: u64, max_nodes: u64) -> Grant {
    let mut grant = grant();
    grant.limits.max_work = max_work;
    grant.limits.max_nodes = max_nodes;
    grant
}

#[test]
fn closure_shapes_keep_their_pinned_budgets() {
    let ((), report) = run_async_under_lab(0x5ec0_6102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let db = database(&commit).await;
        let issuer = authority();
        let wide = 10_000_000;
        let admitted = |query: &PreparedGraphAggregate, limits: [u64; 5]| {
            let token = issuer
                .issue_at(&limited(limits[3], limits[4]), 100)
                .unwrap();
            let policy = GqlQueryPolicy::new(limits[0], 1_000, limits[1], limits[2]);
            db.execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                query,
                policy,
                || 100,
            )
            .is_ok()
        };
        // The least budget in one dimension, the others wide.
        let least = |query: &PreparedGraphAggregate, dimension: usize| {
            let at = |value| {
                let mut limits = [wide; 5];
                limits[dimension] = value;
                limits
            };
            let (mut low, mut high) = (0, wide);
            assert!(admitted(query, at(high)));
            while low < high {
                let middle = low + (high - low) / 2;
                if admitted(query, at(middle)) {
                    high = middle;
                } else {
                    low = middle + 1;
                }
            }
            low
        };
        let mut measured = Vec::new();
        for (text, _) in PINNED {
            let query = prepare(text);
            measured.push([0, 1, 2, 3, 4].map(|dimension| least(&query, dimension)));
        }
        let pinned: Vec<[u64; 5]> = PINNED.iter().map(|&(_, limits)| limits).collect();
        assert_eq!(measured, pinned);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// The same shapes read by the owner, whose closure charges its own walk:
/// least snapshot records, evaluator work and evaluator scratch, recorded at
/// 13e2c5fa (fgdb-61p1n).
const OWNER_PINNED: [[u64; 3]; 6] = [
    [9, 166, 48],
    [16, 184, 58],
    [13, 153, 48],
    [14, 444, 83],
    [8, 142, 42],
    [13, 361, 86],
];

#[test]
fn owner_closure_shapes_keep_their_pinned_budgets() {
    let ((), report) = run_async_under_lab(0x5ec0_6103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let db = database(&commit).await;
        let wide = 10_000_000;
        let admitted = |text: &str, limits: [u64; 3]| {
            let policy = GqlQueryPolicy::new(limits[0], 1_000, limits[1], limits[2]);
            db.query(&cx, text, &GqlParameters::new(), symbols, policy)
                .is_ok()
        };
        let least = |text: &str, dimension: usize| {
            let at = |value| {
                let mut limits = [wide; 3];
                limits[dimension] = value;
                limits
            };
            let (mut low, mut high) = (0, wide);
            assert!(admitted(text, at(high)), "{text}");
            while low < high {
                let middle = low + (high - low) / 2;
                if admitted(text, at(middle)) {
                    high = middle;
                } else {
                    low = middle + 1;
                }
            }
            low
        };
        let measured: Vec<[u64; 3]> = PINNED
            .iter()
            .map(|&(text, _)| [0, 1, 2].map(|dimension| least(text, dimension)))
            .collect();
        assert_eq!(measured, OWNER_PINNED);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
