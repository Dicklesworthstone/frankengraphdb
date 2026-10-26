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

mod write_overlay {
    use super::*;
    use fgdb::{EdgeRecord, VertexRow, WriteTxnError};
    use fgdb_delta_types::LabelId;
    use fgdb_gql::{
        GraphMutationProgramDimension, GraphMutationProgramError, GraphWriteProgramError,
        GraphWriteProgramPolicy, GraphWriteProgramReceipt, GraphWriteScriptExecutionError,
        PreparedGraphWriteScript,
    };
    use fgdb_types::EmbeddedTxnCompletion;
    use fgdb_warden::Rights;

    const L: LabelId = LabelId(1);
    const HIDDEN: LabelId = LabelId(9);
    const Q: PropertyKeyId = PropertyKeyId(2);
    const SECRET: PropertyKeyId = PropertyKeyId(9);
    const UNRELATED_VERTICES: u128 = 512;
    type Fault = GraphWriteScriptExecutionError<WriteTxnError, WriteTxnError, WriteTxnError>;
    type ResultGraph = (GraphWriteProgramReceipt, Vec<VertexRow>, Vec<EdgeRecord>);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            _ => super::symbols(kind, name),
        }
    }

    fn grant(scoped: bool) -> Grant {
        let mut grant = super::grant(1_000_000);
        grant.rights = Rights::ReadWrite;
        if scoped {
            grant.labels = Scope::only([L]);
            grant.relations = Scope::only([R]);
            grant.properties = Scope::only([P, Q]);
        }
        grant
    }

    fn policy(records: u64) -> GraphWriteProgramPolicy {
        GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(records, 10_000, 10_000_000, 1_000_000),
            32,
            8,
            8,
        )
    }

    async fn fixture(commit: &CommitCx, hidden: bool) -> Database<MemVfs> {
        let mut db = Database::open_memory(commit, DatabaseKeys::new([0x92; 32], NS, [0x93; 32]))
            .await
            .unwrap();
        let mut batch = WriteBatch::new(R);
        for id in (1..=4).chain(1_000..1_000 + UNRELATED_VERTICES) {
            let mut labels = vec![L];
            let mut props = vec![
                (P, CanonicalScalar::Int(id as i64)),
                (Q, CanonicalScalar::Int(0)),
            ];
            if hidden {
                labels.push(HIDDEN);
                props.push((SECRET, CanonicalScalar::Int(700)));
            }
            batch.create_vertex(VId(id), labels, props);
        }
        batch.add_edge(EId(1), VId(1), VId(2), vec![(Q, CanonicalScalar::Int(12))]);
        batch.add_edge(EId(2), VId(2), VId(3), vec![(Q, CanonicalScalar::Int(23))]);
        for id in 1_000..1_000 + UNRELATED_VERTICES - 1 {
            batch.add_edge(
                EId(id),
                VId(id),
                VId(id + 1),
                vec![(Q, CanonicalScalar::Int(1))],
            );
        }
        if hidden {
            // These satisfy the unmasked root predicate and touch the visible
            // component. A scoped selection must mask them before charging.
            batch.create_vertex(
                VId(9_000),
                vec![HIDDEN],
                vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(900))],
            );
            batch.add_edge(
                EId(9_000),
                VId(9_000),
                VId(2),
                vec![(Q, CanonicalScalar::Int(999))],
            );
        }
        db.write(commit, batch).await.unwrap();
        if hidden {
            let mut other = WriteBatch::new(S);
            other.add_edge(EId(9_001), VId(1), VId(2), vec![]);
            db.write(commit, other).await.unwrap();
        }
        db
    }

    async fn run(
        contexts: &PurposeContexts,
        source: &str,
        records: u64,
        scoped: bool,
        hidden: bool,
    ) -> Result<ResultGraph, Box<Fault>> {
        let (txn, query, commit) = (contexts.txn(), contexts.query(), contexts.commit());
        let issuer = authority();
        let token = issuer.issue_at(&grant(scoped), 100).unwrap();
        let mut db = fixture(&commit, hidden).await;
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let script = PreparedGraphWriteScript::prepare(source, R, symbols).unwrap();
        let result = db
            .execute_graph_write_script_returning_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &script,
                &GqlParameters::new(),
                policy(records),
                || 100,
            )
            .await;
        assert_eq!(txn.outstanding_obligations(), 0);
        match result {
            Ok((receipt, completion)) => {
                let after = db.frontier().unwrap();
                assert_eq!(after.0, before.0.0 + 1);
                assert_eq!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted { commit_seq: after }
                );
                Ok((receipt, db.vertices().unwrap(), db.edges().unwrap()))
            }
            Err(error) => {
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before,
                    "a late refusal must not publish the earlier statement"
                );
                Err(Box::new(error))
            }
        }
    }

    fn property(vertices: &[VertexRow], vid: u128, key: PropertyKeyId) -> CanonicalScalar {
        vertices
            .iter()
            .find(|row| row.vid == VId(vid))
            .unwrap()
            .props
            .iter()
            .find(|(property, _)| *property == key)
            .unwrap()
            .1
            .as_scalar()
            .unwrap()
            .clone()
    }

    fn assert_record_refusal(error: &Fault) {
        assert!(
            matches!(
                error,
                Fault::Program(GraphWriteProgramError::Program(
                    GraphMutationProgramError::Budget {
                        dimension: GraphMutationProgramDimension::SnapshotRecords,
                        ..
                    }
                ))
            ),
            "{error:?}"
        );
    }

    const SELECTIVE: &str = "MATCH (n {p:1}) SET n.q=10;
         MATCH (a {p:1})-[r:R]->(b) SET b.q=a.q+r.q;
         MATCH (a {p:1})-[:R]->(b)-[:R]->(c) SET c.q=b.q;
         MATCH (a {p:1}),(b {p:3}) SET a.q=b.q";

    #[test]
    fn selective_statements_keep_indexed_admission_after_the_first_write() {
        let ((), report) = run_async_under_lab(0x5ec0_9110, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (receipt, vertices, _) = run(&contexts, SELECTIVE, 11, false, false).await.unwrap();
            assert_eq!(receipt.stats().completed_statements, 4);
            // One root, the one-hop closure, the two-hop closure, then the
            // union of two independent keyed roots: 1 + 3 + 5 + 2.
            assert_eq!(receipt.stats().selection.snapshot_records, 11);
            for id in [1, 2, 3] {
                assert_eq!(property(&vertices, id, Q), CanonicalScalar::Int(22));
            }
            assert_eq!(property(&vertices, 4, Q), CanonicalScalar::Int(0));
            assert!(
                vertices
                    .iter()
                    .filter(|row| row.vid.0 >= 1_000)
                    .all(|row| row.props.contains(&(Q, CanonicalScalar::Int(0).into())))
            );
            assert_record_refusal(
                &run(&contexts, SELECTIVE, 10, false, false)
                    .await
                    .unwrap_err(),
            );
            let unbound = "MATCH (n {p:1}) SET n.q=10;
                           MATCH (a)-[r:R]->(b) SET b.q=a.q+r.q";
            assert_record_refusal(&run(&contexts, unbound, 11, false, false).await.unwrap_err());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn indexed_overlay_sees_staged_edges_retirements_and_rekeyed_roots() {
        let ((), report) = run_async_under_lab(0x5ec0_9111, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let text = "MATCH (a {p:1}),(b {p:4}) CREATE (a)-[:R {q:14}]->(b);
                        MATCH (a {p:1})-[r:R]->(b {p:2}) DELETE r;
                        MATCH (a {p:1}) SET a.p=10;
                        MATCH (a {p:10})-[r:R]->(b) SET b.q=r.q";
            let (receipt, vertices, edges) = run(&contexts, text, 16, false, false).await.unwrap();
            assert_eq!(receipt.stats().completed_statements, 4);
            assert_eq!(receipt.stats().created_edges, 1);
            assert!(receipt.stats().selection.snapshot_records <= 16);
            assert_eq!(property(&vertices, 1, P), CanonicalScalar::Int(10));
            assert_eq!(property(&vertices, 2, Q), CanonicalScalar::Int(0));
            assert_eq!(property(&vertices, 4, Q), CanonicalScalar::Int(14));
            assert!(edges.iter().all(|edge| edge.entry.eid != EId(1)));
            let outgoing: Vec<_> = edges
                .iter()
                .filter(|edge| edge.entry.src == VId(1))
                .collect();
            assert_eq!(outgoing.len(), 1);
            assert_eq!(outgoing[0].entry.dst, VId(4));
            assert_eq!(
                outgoing[0].props,
                vec![(Q, CanonicalScalar::Int(14).into())]
            );

            let created = "CREATE (n {p:99,q:0}); MATCH (n {p:99}) SET n.q=7";
            let (receipt, vertices, _) = run(&contexts, created, 1, false, false).await.unwrap();
            assert_eq!(receipt.stats().selection.snapshot_records, 1);
            let fresh = receipt.steps()[0].created_vertices().unwrap()[0];
            assert_eq!(property(&vertices, fresh.0, Q), CanonicalScalar::Int(7));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn scoped_overlay_keeps_hidden_data_independent_admission_and_refusals() {
        let ((), report) = run_async_under_lab(0x5ec0_9112, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut expected = None;
            for hidden in [false, true] {
                let (receipt, vertices, _) = run(&contexts, SELECTIVE, 10_000, true, hidden)
                    .await
                    .unwrap();
                assert!(receipt.stats().selection.snapshot_records > UNRELATED_VERTICES as u64);
                for id in [1, 2, 3] {
                    assert_eq!(property(&vertices, id, Q), CanonicalScalar::Int(22));
                }
                if hidden {
                    assert_eq!(property(&vertices, 9_000, Q), CanonicalScalar::Int(900));
                    assert_eq!(Some(receipt), expected);
                } else {
                    expected = Some(receipt);
                }
                assert_record_refusal(
                    &run(&contexts, SELECTIVE, 11, true, hidden)
                        .await
                        .unwrap_err(),
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
