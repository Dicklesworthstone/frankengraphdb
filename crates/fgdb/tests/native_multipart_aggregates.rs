//! Native hosts must execute the complete MATCH continuation relation, not
//! send a multi-source aggregate to the exactly-one-source iterator adapter.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryError, QueryResult, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const N: PropertyKeyId = PropertyKeyId(1);
const INNER: &str = "MATCH (owner)-[:R]->(bridge) WITH owner, bridge \
    MATCH (bridge)-[:S]->(item) WITH owner, item.n AS amount \
    RETURN owner, COUNT(*) AS paths, SUM(amount) AS total, COUNT(amount) AS present \
    ORDER BY owner";
const OPTIONAL: &str = "MATCH (owner)-[:R]->(bridge) WITH owner, bridge \
    OPTIONAL MATCH (bridge)-[:S]->(item) WITH owner, item.n AS amount \
    RETURN owner, COUNT(*) AS paths, SUM(amount) AS total, COUNT(amount) AS present \
    ORDER BY owner";

type Summary = (VId, u64, Option<i128>, u64);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(N)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 100_000)
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut vertices = WriteBatch::new(RelationId(9));
    for id in 1..=6 {
        let props = match id {
            2 => vec![(N, CanonicalScalar::Int(10))],
            3 => vec![(N, CanonicalScalar::Int(7))],
            _ => vec![],
        };
        vertices.create_vertex(VId(id), vec![], props);
    }
    let mut first = WriteBatch::new(R);
    for (eid, src, dst) in [(10, 1, 2), (11, 1, 2), (12, 4, 2), (13, 6, 6)] {
        first.add_edge(EId(eid), VId(src), VId(dst), vec![]);
    }
    let mut second = WriteBatch::new(S);
    for (eid, dst) in [(20, 3), (21, 3), (22, 5)] {
        second.add_edge(EId(eid), VId(2), VId(dst), vec![]);
    }
    db.write_atomic(cx, vec![vertices, first, second])
        .await
        .unwrap();
}

fn summaries(result: &QueryResult) -> Vec<Summary> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("read returned a write receipt");
    };
    assert_eq!(columns, &["owner", "paths", "total", "present"]);
    rows.iter()
        .map(|row| {
            (
                row[0].as_value().unwrap().as_vertex().unwrap(),
                row[1].as_count().unwrap(),
                row[2].as_integer(),
                row[3].as_count().unwrap(),
            )
        })
        .collect()
}

#[test]
fn native_multipart_aggregate_reads_and_certified_replay_pin_all_sources() {
    let ((), report) = run_async_under_lab(0xa680_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(INNER, &params, symbols).unwrap();
        let PreparedNativeRead::PipelineAggregate(plan) = &prepared else {
            panic!("multipart aggregate was misclassified");
        };
        assert_eq!(plan.graph_source_count(), 2);
        assert!(plan.requires_relational_input());
        assert!(!plan.is_source_free());
        assert!(plan.bind_parameters(&params).is_err());
        assert!(plan.bind_relation_parameters(&params).is_ok());

        // Independent fixture arithmetic: two R edges times three S edges,
        // with two non-null 7s on the S side; owner 4 has one R edge.
        let expected = vec![(VId(1), 6, Some(28), 4), (VId(4), 3, Some(14), 2)];
        let pinned = db.read_session().unwrap();
        let (certified, certificate) = db
            .execute_certified(&cx, INNER, &params, symbols, policy())
            .unwrap();
        assert_eq!(summaries(&certified), expected);
        assert_eq!(
            prepared.execute(&db, &cx, &params, policy()).unwrap(),
            certified
        );
        assert_eq!(
            db.query(&cx, INNER, &params, symbols, policy()).unwrap(),
            certified
        );
        assert_eq!(
            pinned.query(&cx, INNER, &params, symbols, policy()).unwrap(),
            certified
        );

        // Change a property used only by the second source. Neither the pinned
        // relation nor certified replay may reopen that source at the new cut.
        let mut change = WriteBatch::new(S);
        change.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(11)));
        db.write_atomic(&commit, vec![change]).await.unwrap();
        assert_eq!(
            summaries(&prepared.execute(&db, &cx, &params, policy()).unwrap()),
            vec![(VId(1), 6, Some(44), 4), (VId(4), 3, Some(22), 2)]
        );
        assert_eq!(
            prepared
                .execute_in_view(&pinned, &cx, &params, policy())
                .unwrap(),
            certified
        );
        assert_eq!(
            db.replay(&cx, &certificate, &params, symbols, policy())
                .unwrap(),
            certified
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_optional_aggregate_preserves_null_extension_and_input_pages() {
    let ((), report) = run_async_under_lab(0xa680_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let params = GqlParameters::new();
        let expected = vec![
            (VId(1), 6, Some(28), 4),
            (VId(4), 3, Some(14), 2),
            (VId(6), 1, None, 0),
        ];
        let view = db.read_session().unwrap();
        assert_eq!(
            summaries(&db.query(&cx, OPTIONAL, &params, symbols, policy()).unwrap()),
            expected
        );
        assert_eq!(
            summaries(&view.query(&cx, OPTIONAL, &params, symbols, policy()).unwrap()),
            expected
        );
        let text = "MATCH (owner)-[:R]->(bridge) \
            WITH DISTINCT owner, bridge ORDER BY owner SKIP $skip LIMIT $take \
            OPTIONAL MATCH (bridge)-[:S]->(item) WITH owner, item.n AS amount \
            RETURN owner, COUNT(*) AS paths, SUM(amount) AS total, COUNT(amount) AS present \
            HAVING paths >= $minimum ORDER BY owner";
        let arguments = |skip, minimum| {
            GqlParameters::new()
                .with_uint64("skip", skip)
                .unwrap()
                .with_uint64("take", 1)
                .unwrap()
                .with_int64("minimum", minimum)
                .unwrap()
        };
        let prepared = PreparedNativeRead::prepare(text, &arguments(0, 1), symbols).unwrap();
        for (skip, minimum, expected) in [
            (0, 1, vec![(VId(1), 3, Some(14), 2)]),
            (1, 1, vec![(VId(4), 3, Some(14), 2)]),
            (2, 1, vec![(VId(6), 1, None, 0)]),
            (2, 2, vec![]),
            (3, 1, vec![]),
        ] {
            let args = arguments(skip, minimum);
            assert_eq!(
                summaries(&prepared.execute(&db, &cx, &args, policy()).unwrap()),
                expected
            );
            assert_eq!(
                summaries(&prepared.execute_in_view(&view, &cx, &args, policy()).unwrap()),
                expected
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_multipart_aggregate_budget_refusals_reach_the_executor() {
    let ((), report) = run_async_under_lab(0xa680_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(INNER, &params, symbols).unwrap();
        let view = db.read_session().unwrap();
        for budget in [
            GqlQueryPolicy::new(0, 1_000, 1_000_000, 100_000),
            GqlQueryPolicy::new(1_000, 1_000, 0, 100_000),
        ] {
            assert!(matches!(
                prepared.execute(&db, &cx, &params, budget),
                Err(QueryError::Aggregate(_))
            ));
            assert!(matches!(
                prepared.execute_in_view(&view, &cx, &params, budget),
                Err(QueryError::Aggregate(_))
            ));
        }
        // A refusal cannot poison the reusable template or switch its lane.
        assert_eq!(
            summaries(&prepared.execute(&db, &cx, &params, policy()).unwrap()).len(),
            2
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
