//! An authorized read from a predicate-bound root, under a capability that
//! hides nothing, admits its edge closure and then each distinct endpoint of
//! that closure once (fgdb-61p1n). The hub's neighbours here all point into a
//! small shared set of targets, so most endpoints recur many times; each
//! admitted vertex costs one node, so a node budget sees every repeat.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, QueryResult, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error as Denied, Grant, LimitDimension, QueryLimits, Scope};
use std::collections::BTreeSet;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x91; 32]);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
/// The hub is vertex 0 (p = 0); its neighbours are 1..=NEIGHBOURS.
const NEIGHBOURS: u128 = 40;
/// Neighbour b points at TARGET + (b * k) % TARGETS for k in 0..b % 5 + 1.
const TARGET: u128 = 1_000;
const TARGETS: u128 = 7;
/// Vertices no closure reaches, with R edges among themselves.
const UNRELATED: u128 = 5_000;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1_000, 100_000_000, 10_000_000)
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9101), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
/// A grant that hides nothing, with `max_nodes` the only tight limit.
fn grant(max_nodes: u64) -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits {
            max_nodes,
            max_work: 100_000_000,
            max_rows: 1000,
        },
    );
    grant.labels = Scope::All;
    grant.relations = Scope::All;
    grant.properties = Scope::All;
    grant
}

/// The R edges: hub -> each neighbour, each neighbour -> its targets.
fn r_edges() -> Vec<(u128, u128)> {
    let mut edges: Vec<(u128, u128)> = (1..=NEIGHBOURS).map(|b| (0, b)).collect();
    for b in 1..=NEIGHBOURS {
        for k in 0..b % 5 + 1 {
            edges.push((b, TARGET + (b * k) % TARGETS));
        }
    }
    edges
}

async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x92; 32], NS, [0x93; 32]))
        .await
        .unwrap();
    let mut vertices = WriteBatch::new(R);
    let ids = (0..=NEIGHBOURS)
        .chain(TARGET..TARGET + TARGETS)
        .chain(UNRELATED..UNRELATED + 20);
    for id in ids {
        vertices.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    db.write(cx, vertices).await.unwrap();
    let mut r = WriteBatch::new(R);
    for (at, &(from, to)) in r_edges().iter().enumerate() {
        r.add_edge(EId(at as u128 + 1), VId(from), VId(to), vec![]);
    }
    for at in 0..19 {
        r.add_edge(
            EId(10_000 + at),
            VId(UNRELATED + at),
            VId(UNRELATED + at + 1),
            vec![],
        );
    }
    db.write(cx, r).await.unwrap();
    // Another relation from the targets back to the hub: no R closure takes it.
    let mut s = WriteBatch::new(S);
    for t in 0..TARGETS {
        s.add_edge(EId(20_000 + t), VId(TARGET + t), VId(0), vec![]);
    }
    db.write(cx, s).await.unwrap();
    db
}

fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

const TWO_HOP: &str = "MATCH (a {p: 0})-[:R]->(b)-[:R]->(c) RETURN COUNT(*) AS n";

#[test]
fn a_bound_two_hop_admits_each_distinct_endpoint_once() {
    let ((), report) = run_async_under_lab(0x5ec0_9101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let db = database(&commit).await;
        let issuer = authority();
        let query = prepare(TWO_HOP);
        let edges = r_edges();
        let walks = edges
            .iter()
            .filter(|&&(from, _)| from == 0)
            .map(|&(_, b)| edges.iter().filter(|&&(from, _)| from == b).count())
            .sum::<usize>() as u64;
        let mut endpoints = BTreeSet::new();
        for &(from, to) in &edges {
            endpoints.insert(from);
            endpoints.insert(to);
        }
        let occurrences = 2 * edges.len();
        assert!(endpoints.len() * 3 < occurrences, "endpoints recur");

        let run = |max_nodes| {
            let token = issuer.issue_at(&grant(max_nodes), 100).unwrap();
            db.execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                &query,
                policy(),
                || 100,
            )
        };
        let rows = run(1_000_000).unwrap();
        assert_eq!(rows[0].values(), &[GraphAggregateValue::Count(walks)]);
        // The owner path agrees.
        let QueryResult::Rows { rows: owner, .. } = db
            .query(&cx, TWO_HOP, &GqlParameters::new(), symbols, policy())
            .unwrap()
        else {
            panic!("not a row result");
        };
        assert_eq!(owner[0][0].as_count(), Some(walks));

        // Every closure vertex costs one node, once.
        let least = endpoints.len() as u64;
        assert!(run(least).is_ok());
        assert!(matches!(
            run(least - 1),
            Err(QueryError::Authorization(Denied::LimitExceeded(
                LimitDimension::Nodes
            )))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
