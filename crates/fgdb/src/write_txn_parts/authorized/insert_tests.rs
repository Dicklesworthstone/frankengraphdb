//! Public creation entry points with real tokens, collector, storage and reopen.
use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::insertion::{GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphInsertText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const H: RelationId = RelationId(3);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x47; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x46; 32], NS, [0x48; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9927), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R, S]),
        properties: Scope::only([P]),
        rights: Rights::Write,
        limits: QueryLimits { max_nodes: 100_000, max_work: 1_000_000, max_rows: 100 },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 100_000), 100, 100)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Relation, "Hidden") => Some(GraphSymbol::Relation(H)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn insert(text: &str) -> PreparedGraphInsert {
    PreparedGraphInsertText::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn authorization(error: Fault) -> Error {
    match error {
        GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        | GqlQueryError::Source(GraphInsertError::Source(WriteTxnError::Authorization(error))) => error,
        other => panic!("expected a typed authorization failure, got {other:?}"),
    }
}
fn lab<T, F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let (result, report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
    result
}

#[test]
fn engine_issued_multi_relation_creation_is_atomic_and_reopens() {
    lab(0xa931, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let mut seed = WriteBatch::new(H);
        seed.create_vertex(VId(900), vec![HIDDEN], vec![(SECRET, CanonicalScalar::Int(99))]);
        seed.create_vertex(VId(901), vec![HIDDEN], vec![]);
        seed.add_edge(EId(800), VId(900), VId(901), vec![]);
        db.write(&commit, seed).await.unwrap();
        let mut retire = WriteBatch::new(H);
        retire.delete_vertex(VId(901));
        db.write(&commit, retire).await.unwrap();
        let frontier = db.frontier().unwrap();
        let hidden = db.vertex(VId(900)).unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let template = PreparedGraphInsertText::prepare(
            "INSERT (a:Visible {p:$value}), (b:Visible), (a)-[:R]->(b), (b)-[:S {p:7}]->(a), (a)-[:S]->(a)",
            R, symbols,
        ).unwrap();
        let insertion = template.bind_parameters(&GqlParameters::new().with_int64("value", 42).unwrap()).unwrap();
        let baseline = txn.outstanding_obligations();
        let (stats, vertices, edges, completion) = db.execute_graph_insert_returning_authorized(
            &txn, &query, &commit, &authority, &token, "main", &insertion, policy(), || NOW,
        ).await.unwrap();
        assert_eq!(stats.created_vertices, 2);
        assert_eq!(stats.created_edges, 3);
        assert_eq!(stats.selection.snapshot_records, 0);
        assert_eq!(vertices.len(), 2);
        assert_eq!(edges.len(), 3);
        assert!(vertices.iter().all(|id| id.0 > 901));
        assert!(edges.iter().all(|id| id.0 > 800));
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, frontier.0 + 1);
        assert_eq!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq });
        assert_eq!(txn.outstanding_obligations(), baseline);
        assert_eq!(db.vertex(VId(900)).unwrap(), hidden);
        let all_vertices = db.vertices().unwrap();
        let all_edges = db.edges().unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(db.vertices().unwrap(), all_vertices);
        assert_eq!(db.edges().unwrap(), all_edges);
        assert_eq!(db.vertex(vertices[0]).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(42))]);
        for (index, source, relation, destination) in [
            (0, vertices[0], R, vertices[1]),
            (1, vertices[1], S, vertices[0]),
            (2, vertices[0], S, vertices[0]),
        ] {
            let edge = db.edge(edges[index]).unwrap().unwrap();
            assert_eq!((edge.entry.src, edge.entry.relation, edge.entry.dst), (source, relation, destination));
        }
    });
}

#[test]
fn forbidden_creation_tail_aborts_every_relation_and_retries_without_reusing_ids() {
    lab(0xa932, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for tail in ["(b)-[:Hidden]->(a)", "(b)-[:S {secret:9}]->(a)"] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let frontier = db.frontier().unwrap();
            let baseline = txn.outstanding_obligations();
            let bad = insert(&format!("INSERT (a:Visible), (b:Visible), (a)-[:R]->(b), {tail}"));
            let error = db.execute_graph_insert_returning_authorized(
                &txn, &query, &commit, &authority, &token, "main", &bad, policy(), || NOW,
            ).await.unwrap_err();
            assert_eq!(authorization(error), Error::ScopeDenied);
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), baseline);
            let good = insert("CREATE (a:Visible), (b:Visible), (a)-[:S]->(b)");
            let (_, vertices, edges, _) = db.execute_graph_insert_returning_authorized(
                &txn, &query, &commit, &authority, &token, "main", &good, policy(), || NOW,
            ).await.unwrap();
            assert!(vertices.iter().all(|id| id.0 > 2));
            assert!(edges.iter().all(|id| id.0 > 2));
            assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
            assert_eq!(txn.outstanding_obligations(), baseline);
        }
    });
}

#[test]
fn returned_identity_rows_are_admitted_before_commit_but_stats_need_no_row_budget() {
    lab(0xa933, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let insertion = insert("INSERT (a:Visible), (b:Visible), (a)-[:R]->(b)");
        for rows in [0, 1, 2, 3] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let frontier = db.frontier().unwrap();
            let limited = token.attenuate(Restriction::MaxRows(rows)).unwrap();
            let result = db.execute_graph_insert_returning_authorized(
                &txn, &query, &commit, &authority, &limited, "main", &insertion, policy(), || NOW,
            ).await;
            if rows < 3 {
                assert_eq!(authorization(result.unwrap_err()), Error::LimitExceeded(LimitDimension::Rows));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertices().unwrap().is_empty());
                assert!(db.edges().unwrap().is_empty());
            } else {
                let (_, vertices, edges, _) = result.unwrap();
                assert_eq!(vertices.len() + edges.len(), 3);
                assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let (stats, _) = db.execute_graph_insert_authorized(
            &txn, &query, &commit, &authority, &zero, "main", &insertion, policy(), || NOW,
        ).await.unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (2, 1));
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn permission_and_creation_limits_precede_identity_issuance() {
    lab(0xa934, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let insertion = insert("INSERT (a:Visible)");
        for read_only in [true, false] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let frontier = db.frontier().unwrap();
            let mut grant = grant();
            let mut policy = policy();
            if read_only {
                grant.rights = Rights::Read;
            } else {
                policy.max_vertices = 0;
            }
            let token = authority.issue_at(&grant, NOW).unwrap();
            let error = db.execute_graph_insert_authorized(
                &txn, &query, &commit, &authority, &token, "main", &insertion, policy, || NOW,
            ).await.unwrap_err();
            if read_only {
                assert_eq!(authorization(error), Error::PermissionDenied);
            } else {
                assert!(matches!(error, GqlQueryError::Source(GraphInsertError::Limit {
                    dimension: GraphInsertLimitDimension::Vertices, limit: 0, observed: 1,
                })));
            }
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(txn.outstanding_obligations(), 0);
            assert_eq!(db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 }).unwrap(), ElementId::Vertex(VId(1)));
        }
    });
}

#[test]
fn signed_work_and_node_limits_cover_collection_and_native_creation_together() {
    lab(0xa935, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let insertion = insert("INSERT (a:Visible {p:7}), (b:Visible), (a)-[:S]->(b)");
        for dimension in [LimitDimension::Work, LimitDimension::Nodes] {
            let (mut low, mut high) = (0_u64, 1024_u64);
            while low < high {
                let middle = low + (high - low) / 2;
                let restriction = match dimension {
                    LimitDimension::Work => Restriction::MaxWork(middle),
                    LimitDimension::Nodes => Restriction::MaxNodes(middle),
                    _ => unreachable!(),
                };
                let limited = token.attenuate(restriction).unwrap();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let frontier = db.frontier().unwrap();
                match db.execute_graph_insert_authorized(
                    &txn, &query, &commit, &authority, &limited, "main", &insertion, policy(), || NOW,
                ).await {
                    Ok((stats, _)) => {
                        assert_eq!((stats.created_vertices, stats.created_edges), (2, 1));
                        high = middle;
                    }
                    Err(error) => {
                        assert_eq!(authorization(error), Error::LimitExceeded(dimension));
                        assert_eq!(db.frontier().unwrap(), frontier);
                        assert!(db.vertices().unwrap().is_empty());
                        assert!(db.edges().unwrap().is_empty());
                        low = middle + 1;
                    }
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
            // Not merely the three creations: every before/after admission and
            // every collector/preparation event uses this same allowance.
            assert!(low > 3 && low < 1024, "{dimension:?} threshold {low}");
        }
    });
}
