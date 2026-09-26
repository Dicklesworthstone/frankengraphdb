//! Public mixed-program entry points: one token, overlay and native completion.
use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::insertion::{GraphInsertError, GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphDeleteError, GraphSymbol, GraphSymbolKind,
    PreparedGraphDeleteText, PreparedGraphInsertText, PreparedGraphMutationText,
    PreparedGraphVertexMergeText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x73; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x72; 32], NS, [0x74; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9973), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(), labels: Scope::All, relations: Scope::All,
        properties: Scope::All, rights: Rights::ReadWrite,
        limits: QueryLimits { max_nodes: 100_000, max_work: 1_000_000, max_rows: 100 },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 100_000), 100, 100, 100)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn insert(text: &str) -> GraphWriteStatement {
    PreparedGraphInsertText::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap().into()
}
fn mutation(text: &str) -> GraphWriteStatement {
    PreparedGraphMutationText::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap().into()
}
fn deletion(text: &str) -> GraphWriteStatement {
    PreparedGraphDeleteText::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap().into()
}
fn mixed() -> PreparedGraphWriteProgram {
    PreparedGraphWriteProgram::prepare(vec![
        insert("CREATE (a:Visible {p:10}), (b:Visible {p:20}), (a)-[:R {p:1}]->(b)"),
        insert("MATCH (a:Visible)-[:R]->(b:Visible) \
            INSERT (copy:Visible {p:a.p + b.p}), (b)-[:S {p:2}]->(copy)"),
        mutation("MATCH (a:Visible)-[e:S]->(b:Visible) SET a.p = b.p + 1, e.p = 9"),
        deletion("MATCH (a:Visible)-[e:R]->(b:Visible) DELETE e"),
        deletion("MATCH (a:Visible) WHERE a.p = 10 DELETE a"),
    ]).unwrap()
}
fn authorization(error: Fault) -> Error {
    use GraphMutationProgramError as M;
    match error {
        Fault::Program(M::Preflight(WriteTxnError::Authorization(error)))
        | Fault::Program(M::Interrupted { source: WriteTxnError::Authorization(error), .. })
        | Fault::Program(M::Statement { source: GqlQueryError::Interrupted(WriteTxnError::Authorization(error)), .. })
        | Fault::Program(M::Statement { source: GqlQueryError::Source(GraphMutationError::Source(WriteTxnError::Authorization(error))), .. })
        | Fault::Insert { source: GqlQueryError::Interrupted(WriteTxnError::Authorization(error)), .. }
        | Fault::Insert { source: GqlQueryError::Source(GraphInsertError::Source(WriteTxnError::Authorization(error))), .. }
        | Fault::Delete { source: GqlQueryError::Interrupted(WriteTxnError::Authorization(error)), .. }
        | Fault::Delete { source: GqlQueryError::Source(GraphDeleteError::Source(WriteTxnError::Authorization(error))), .. } => error,
        other => panic!("expected authorization failure, got {other:?}"),
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
fn dependent_cross_relation_crud_has_ordered_receipts_one_commit_and_reopen() {
    lab(0xa981, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let (receipt, completion) = db.execute_graph_write_program_returning_authorized(
            &txn, &query, &commit, &authority, &token, "main", &mixed(), policy(), || NOW,
        ).await.unwrap();
        let stats = receipt.stats();
        assert_eq!((stats.completed_statements, stats.created_vertices, stats.created_edges, stats.mutation_effects), (5, 3, 2, 4));
        assert_eq!(receipt.steps(), &[
            GraphWriteStepReceipt::Insert { vertices: vec![VId(1), VId(2)], edges: vec![EId(1)] },
            GraphWriteStepReceipt::Insert { vertices: vec![VId(3)], edges: vec![EId(2)] },
            GraphWriteStepReceipt::Mutation { targets: vec![VId(2)], edges: vec![EId(2)] },
            GraphWriteStepReceipt::Delete { targets: vec![], edges: vec![EId(1)] },
            GraphWriteStepReceipt::Delete { targets: vec![VId(1)], edges: vec![] },
        ]);
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, before.0 + 1);
        assert_eq!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq });
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert_eq!(db.vertex(VId(2)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(31))]);
        assert_eq!(db.vertex(VId(3)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(30))]);
        assert!(db.edge(EId(1)).unwrap().is_none());
        let edge = db.edge(EId(2)).unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.relation, edge.entry.dst), (VId(2), S, VId(3)));
        assert_eq!(edge.props, vec![(P, CanonicalScalar::Int(9))]);
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(db.edges().unwrap(), edges);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn denied_tail_discards_created_prefix_without_reclaiming_issued_ids() {
    lab(0xa982, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let authority = authority();
        let mut grant = grant();
        grant.properties = Scope::only([P]);
        let token = authority.issue_at(&grant, NOW).unwrap();
        let program = PreparedGraphWriteProgram::prepare(vec![
            insert("INSERT (a:Visible {p:10})"),
            mutation("MATCH (a:Visible) WHERE a.p = 10 SET a.p = 11, a.secret = 99"),
        ]).unwrap();
        let error = db.execute_graph_write_program_returning_authorized(
            &txn, &query, &commit, &authority, &token, "main", &program, policy(), || NOW,
        ).await.unwrap_err();
        assert_eq!(authorization(error), Error::ScopeDenied);
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty() && db.edges().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
        let retry = PreparedGraphWriteProgram::prepare(vec![insert("INSERT (a:Visible {p:10})")]).unwrap();
        let (receipt, _) = db.execute_graph_write_program_returning_authorized(
            &txn, &query, &commit, &authority, &token, "main", &retry, policy(), || NOW,
        ).await.unwrap();
        assert_eq!(receipt.steps()[0].created_vertices(), Some(&[VId(2)][..]));
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn receipt_row_budget_accumulates_across_kinds_before_publication() {
    lab(0xa983, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        // The five receipts contain 3 + 2 + 2 + 1 + 1 identities. Reusing an
        // identity in a later step cannot restart or deduplicate its row bill.
        for rows in [0, 3, 5, 7, 8, 9] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let limited = token.attenuate(Restriction::MaxRows(rows)).unwrap();
            let result = db.execute_graph_write_program_returning_authorized(
                &txn, &query, &commit, &authority, &limited, "main", &mixed(), policy(), || NOW,
            ).await;
            if rows < 9 {
                assert_eq!(authorization(result.unwrap_err()), Error::LimitExceeded(LimitDimension::Rows));
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty() && db.edges().unwrap().is_empty());
            } else {
                let (receipt, _) = result.unwrap();
                assert_eq!(receipt.steps().len(), 5);
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let (stats, _) = db.execute_graph_write_program_authorized(
            &txn, &query, &commit, &authority, &zero, "main", &mixed(), policy(), || NOW,
        ).await.unwrap();
        assert_eq!(stats.completed_statements, 5);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn creation_quota_counts_a_vertex_even_after_a_later_delete() {
    lab(0xa984, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let program = PreparedGraphWriteProgram::prepare(vec![
            insert("INSERT (a:Visible {p:1})"),
            deletion("MATCH (a:Visible) WHERE a.p = 1 DELETE a"),
            insert("INSERT (b:Visible {p:2})"),
        ]).unwrap();
        let limited = GraphWriteProgramPolicy { max_created_vertices: 1, ..policy() };
        let error = db.execute_graph_write_program_authorized(
            &txn, &query, &commit, &authority, &token, "main", &program, limited, || NOW,
        ).await.unwrap_err();
        assert!(matches!(error, Fault::CreationBudget {
            statement: 2, dimension: GraphInsertLimitDimension::Vertices, limit: 1, observed: 2,
        }));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        // The refused third step never reached the allocator; the first step's
        // reservation is nevertheless not reclaimed by the program rollback.
        assert_eq!(db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 }).unwrap(), ElementId::Vertex(VId(2)));
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn plain_delete_proves_incidence_from_staged_creations_and_prior_deletions() {
    lab(0xa985, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for remove_all in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut steps = vec![insert("INSERT (a:Visible), (b:Visible), \
                (a)-[:R {p:1}]->(b), (a)-[:R {p:2}]->(b), (a)-[:S]->(a)")];
            if remove_all {
                steps.push(deletion("MATCH (a:Visible)-[e:S]->(b:Visible) DELETE e"));
                steps.push(deletion("MATCH (a:Visible)-[e:R]->(b:Visible) DELETE a, e, b"));
            } else {
                steps.push(deletion("MATCH (a:Visible)-[e:R]->(b:Visible) WHERE e.p = 1 DELETE a, e"));
            }
            let program = PreparedGraphWriteProgram::prepare(steps).unwrap();
            let result = db.execute_graph_write_program_returning_authorized(
                &txn, &query, &commit, &authority, &token, "main", &program, policy(), || NOW,
            ).await;
            if remove_all {
                let (receipt, _) = result.unwrap();
                assert_eq!(receipt.stats().mutation_effects, 5);
                assert_eq!(receipt.steps()[2].deleted_vertices(), Some(&[VId(1), VId(2)][..]));
                assert_eq!(receipt.steps()[2].deleted_edges(), Some(&[EId(1), EId(2)][..]));
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            } else {
                assert!(matches!(result, Err(Fault::Delete {
                    statement: 1, source: GqlQueryError::Source(GraphDeleteError::IncidentRelationships),
                })));
                assert_eq!(db.frontier().unwrap(), before);
            }
            assert!(db.vertices().unwrap().is_empty() && db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn whole_program_rights_and_unsupported_tail_are_checked_before_allocating() {
    lab(0xa986, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let full = authority.issue_at(&grant(), NOW).unwrap();
        for mode in 0..3 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut write = grant();
            write.rights = Rights::Write;
            write.limits.max_work = 0;
            let write_only = authority.issue_at(&write, NOW).unwrap();
            let tail = match mode {
                0 => mutation("MATCH (a:Visible) WHERE a.p = 999 SET a.p = 1"),
                1 => insert("MATCH (a:Visible) WHERE a.p = 999 INSERT (b:Visible)"),
                _ => PreparedGraphVertexMergeText::prepare("MERGE (a:Visible {p:7})", R, symbols).unwrap()
                    .bind_parameters(&GqlParameters::new()).unwrap().into(),
            };
            let program = PreparedGraphWriteProgram::prepare(vec![insert("INSERT (a:Visible {p:10})"), tail]).unwrap();
            let token = if mode == 2 { &full } else { &write_only };
            let error = db.execute_graph_write_program_authorized(
                &txn, &query, &commit, &authority, token, "main", &program, policy(), || NOW,
            ).await.unwrap_err();
            if mode == 2 {
                assert!(matches!(error, Fault::Program(GraphMutationProgramError::Preflight(WriteTxnError::AuthorizedMutationRefused))));
            } else {
                assert_eq!(authorization(error), Error::PermissionDenied);
            }
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 }).unwrap(), ElementId::Vertex(VId(1)));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        // Write-only authority remains useful: no implicit Read privilege is
        // needed for a program of entirely standalone engine-owned creations.
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut write = grant();
        write.rights = Rights::Write;
        write.limits.max_rows = 0;
        let token = authority.issue_at(&write, NOW).unwrap();
        let program = PreparedGraphWriteProgram::prepare(vec![
            insert("INSERT (a:Visible {p:1})"), insert("CREATE (b:Visible {p:2})"),
        ]).unwrap();
        let (stats, _) = db.execute_graph_write_program_authorized(
            &txn, &query, &commit, &authority, &token, "main", &program, policy(), || NOW,
        ).await.unwrap();
        assert_eq!((stats.completed_statements, stats.created_vertices), (2, 2));
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn empty_mixed_selection_closes_without_a_marker_or_id_reservation() {
    lab(0xa987, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap()
            .attenuate(Restriction::MaxRows(0)).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let program = PreparedGraphWriteProgram::prepare(vec![
            insert("MATCH (a:Visible) INSERT (copy:Visible)"),
            mutation("MATCH (a:Visible) SET a.p = 1"),
            deletion("MATCH (a:Visible) DELETE a"),
        ]).unwrap();
        let (receipt, completion) = db.execute_graph_write_program_returning_authorized(
            &txn, &query, &commit, &authority, &token, "main", &program, policy(), || NOW,
        ).await.unwrap();
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(receipt.stats().proposed_effects(), 0);
        assert_eq!(receipt.steps().len(), 3);
        assert_eq!(completion, EmbeddedTxnCompletion::ReadClosed { snapshot_seq: before, validated_through: before });
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 }).unwrap(), ElementId::Vertex(VId(1)));
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn creation_steps_share_one_signed_node_allowance() {
    lab(0xa988, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut floors = Vec::new();
        for steps in [1, 2] {
            let program = PreparedGraphWriteProgram::prepare(
                (0..steps).map(|_| insert("INSERT (a:Visible {p:1})")).collect(),
            ).unwrap();
            let (mut low, mut high) = (0_u64, 128_u64);
            while low < high {
                let middle = low + (high - low) / 2;
                let limited = token.attenuate(Restriction::MaxNodes(middle)).unwrap();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let before = db.frontier().unwrap();
                match db.execute_graph_write_program_authorized(
                    &txn, &query, &commit, &authority, &limited, "main", &program, policy(), || NOW,
                ).await {
                    Ok((stats, _)) => {
                        assert_eq!(stats.created_vertices, steps);
                        high = middle;
                    }
                    Err(error) => {
                        assert_eq!(authorization(error), Error::LimitExceeded(LimitDimension::Nodes));
                        assert_eq!(db.frontier().unwrap(), before);
                        assert!(db.vertices().unwrap().is_empty());
                        low = middle + 1;
                    }
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
            assert!(low > 0 && low < 128);
            floors.push(low);
        }
        assert_eq!(floors[1], 2 * floors[0], "per-statement permits reset the node allowance");
    });
}

#[test]
fn expiry_before_final_publication_discards_all_mixed_steps() {
    lab(0xa989, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let calls = AtomicU64::new(0);
        db.execute_graph_write_program_returning_authorized(
            &txn, &query, &commit, &authority, &token, "main", &mixed(), policy(), || {
                calls.fetch_add(1, Ordering::Relaxed);
                NOW
            },
        ).await.unwrap();
        let count = calls.load(Ordering::Relaxed);
        assert!(count > 10);
        for cutoff in [1, count / 3, 2 * count / 3, count - 1] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let calls = AtomicU64::new(0);
            let error = db.execute_graph_write_program_returning_authorized(
                &txn, &query, &commit, &authority, &token, "main", &mixed(), policy(), || {
                    if calls.fetch_add(1, Ordering::Relaxed) < cutoff { NOW } else { 10_000 }
                },
            ).await.unwrap_err();
            assert_eq!(authorization(error), Error::Expired);
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty() && db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}
