//! Public creation entry points with real tokens, collector, storage and reopen.
use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::insertion::{GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphInsertBinding, GraphSetProjection, GraphSetQuantifier,
    GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphInsertText,
};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts};
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
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows: 100,
        },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 100_000),
        100,
        100,
    )
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
    PreparedGraphInsertText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn authorization(error: Fault) -> Error {
    match error {
        GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        | GqlQueryError::Source(GraphInsertError::Source(WriteTxnError::Authorization(error))) => {
            error
        }
        other => panic!("expected a typed authorization failure, got {other:?}"),
    }
}
fn query_authorization(error: &QueryFault) -> Option<Error> {
    let mut cause: Option<&(dyn core::error::Error + 'static)> = Some(error);
    while let Some(error) = cause {
        if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        cause = error.source();
    }
    None
}

fn projected_insert(
    insertion: PreparedGraphInsert,
    bindings: Vec<GraphInsertBinding>,
    quantifier: GraphSetQuantifier,
) -> PreparedGraphInsertQuery {
    let projections = (0..bindings.len())
        .map(|column| GraphSetProjection::new(format!("c{column}"), GraphSetValue::Column(column)))
        .collect();
    PreparedGraphInsertQuery::prepare(insertion, bindings, projections, quantifier).unwrap()
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
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(H);
        seed.create_vertex(
            VId(900),
            vec![HIDDEN],
            vec![(SECRET, CanonicalScalar::Int(99))],
        );
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
        let insertion = template
            .bind_parameters(&GqlParameters::new().with_int64("value", 42).unwrap())
            .unwrap();
        let baseline = txn.outstanding_obligations();
        let (stats, vertices, edges, completion) = db
            .execute_graph_insert_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &insertion,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 2);
        assert_eq!(stats.created_edges, 3);
        assert_eq!(stats.selection.snapshot_records, 0);
        assert_eq!(vertices.len(), 2);
        assert_eq!(edges.len(), 3);
        assert!(vertices.iter().all(|id| id.0 > 901));
        assert!(edges.iter().all(|id| id.0 > 800));
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, frontier.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq }
        );
        assert_eq!(txn.outstanding_obligations(), baseline);
        assert_eq!(db.vertex(VId(900)).unwrap(), hidden);
        let all_vertices = db.vertices().unwrap();
        let all_edges = db.edges().unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(db.vertices().unwrap(), all_vertices);
        assert_eq!(db.edges().unwrap(), all_edges);
        assert_eq!(
            db.vertex(vertices[0]).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(42))]
        );
        for (index, source, relation, destination) in [
            (0, vertices[0], R, vertices[1]),
            (1, vertices[1], S, vertices[0]),
            (2, vertices[0], S, vertices[0]),
        ] {
            let edge = db.edge(edges[index]).unwrap().unwrap();
            assert_eq!(
                (edge.entry.src, edge.entry.relation, edge.entry.dst),
                (source, relation, destination)
            );
        }
    });
}

#[test]
fn relational_creation_preserves_write_only_authority_and_masks_graph_inputs() {
    lab(0xa94a, |contexts| async move {
        use fgdb_gql::insertion::GraphInsertVertex;
        use fgdb_gql::{GraphMutationValue, PreparedGraphSetText};

        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(H);
        seed.create_vertex(VId(900), vec![HIDDEN], vec![(P, CanonicalScalar::Int(99))]);
        db.write(&commit, seed).await.unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let relation = |text: &str| {
            PreparedGraphSetText::prepare(text, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap()
        };
        let insertion = |input| {
            PreparedGraphInsert::prepare_relation(
                input,
                R,
                vec![GraphInsertVertex {
                    labels: vec![L],
                    properties: vec![(P, GraphMutationValue::Column(0))],
                }],
                vec![],
            )
            .unwrap()
        };
        let source_free = insertion(relation("UNWIND [3, 1, 3] AS p RETURN p"));
        assert!(!source_free.requires_read());
        assert!(!source_free.is_standalone());
        let before = db.frontier().unwrap();
        let (stats, vertices, _, completion) = db
            .execute_graph_insert_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &source_free,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.selection.snapshot_records, 0);
        assert_eq!(stats.selection.result_rows, 3);
        assert_eq!(stats.created_vertices, 3);
        assert_eq!(vertices.len(), 3);
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        for (id, expected) in vertices.iter().zip([3, 1, 3]) {
            assert_eq!(
                db.vertex(*id).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(expected))]
            );
        }

        // A relational graph leaf still requires read authority, even when a
        // result window would discard every row. No private workspace or ID
        // allocation is admitted ahead of this structural check.
        let graph_input = relation("MATCH (n) RETURN n.p AS p");
        let denied = insertion(graph_input.clone().with_page(0, Some(0)));
        assert!(denied.requires_read());
        let before_denial = db.frontier().unwrap();
        let error = db
            .execute_graph_insert_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &denied,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(error), Error::PermissionDenied);
        assert_eq!(db.frontier().unwrap(), before_denial);

        let token = authority
            .issue_at(
                &Grant {
                    rights: Rights::ReadWrite,
                    ..grant()
                },
                NOW,
            )
            .unwrap();
        let (stats, copied, _, _) = db
            .execute_graph_insert_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &insertion(graph_input),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            stats.created_vertices, 3,
            "the hidden vertex must never reach CREATE"
        );
        let properties: Vec<_> = copied
            .iter()
            .map(|id| db.vertex(*id).unwrap().unwrap().props)
            .collect();
        // Creation follows the relation's row order. A pattern's value
        // projection arrives in canonical value order (GLA ALL collector;
        // `SetNode::Pattern` does not preserve row order), unlike the
        // source-free UNWIND above, which keeps list order [3, 1, 3].
        assert_eq!(
            properties,
            vec![
                vec![(P, CanonicalScalar::Int(1))],
                vec![(P, CanonicalScalar::Int(3))],
                vec![(P, CanonicalScalar::Int(3))],
            ]
        );
        assert_eq!(
            db.vertex(VId(900)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(99))]
        );
        assert_eq!(txn.outstanding_obligations(), 0);
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
            let bad = insert(&format!(
                "INSERT (a:Visible), (b:Visible), (a)-[:R]->(b), {tail}"
            ));
            let error = db
                .execute_graph_insert_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &bad,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(error), Error::ScopeDenied);
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), baseline);
            let good = insert("CREATE (a:Visible), (b:Visible), (a)-[:S]->(b)");
            let (_, vertices, edges, _) = db
                .execute_graph_insert_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &good,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap();
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
            let result = db
                .execute_graph_insert_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &limited,
                    "main",
                    &insertion,
                    policy(),
                    || NOW,
                )
                .await;
            if rows < 3 {
                assert_eq!(
                    authorization(result.unwrap_err()),
                    Error::LimitExceeded(LimitDimension::Rows)
                );
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
        let (stats, _) = db
            .execute_graph_insert_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &zero,
                "main",
                &insertion,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
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
            let error = db
                .execute_graph_insert_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &insertion,
                    policy,
                    || NOW,
                )
                .await
                .unwrap_err();
            if read_only {
                assert_eq!(authorization(error), Error::PermissionDenied);
            } else {
                assert!(matches!(
                    error,
                    GqlQueryError::Source(GraphInsertError::Limit {
                        dimension: GraphInsertLimitDimension::Vertices,
                        limit: 0,
                        observed: 1,
                    })
                ));
            }
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(txn.outstanding_obligations(), 0);
            assert_eq!(
                db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                    .unwrap(),
                ElementId::Vertex(VId(1))
            );
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
                match db
                    .execute_graph_insert_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &limited,
                        "main",
                        &insertion,
                        policy(),
                        || NOW,
                    )
                    .await
                {
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

fn read_write_grant() -> Grant {
    Grant {
        rights: Rights::ReadWrite,
        ..grant()
    }
}

/// Visible vertices 1 and 2 joined by an R edge, plus one kind of data around
/// vertex 1 the grant cannot see. Variant 0 adds nothing; 1 a hidden-relation
/// edge; 2 an R edge to a hidden vertex; 3 a hidden property on vertex 1; 4 a
/// hidden label on vertex 1; 5 a hidden property on the R edge.
async fn creation_fixture(cx: &CommitCx, hidden: u8) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    let labels = if hidden == 4 {
        vec![L, HIDDEN]
    } else {
        vec![L]
    };
    let secret = |variant| {
        if hidden == variant {
            vec![(SECRET, CanonicalScalar::Int(9))]
        } else {
            vec![]
        }
    };
    seed.create_vertex(VId(1), labels, secret(3));
    seed.create_vertex(VId(2), vec![L], vec![]);
    seed.add_edge(EId(10), VId(1), VId(2), secret(5));
    if hidden == 2 {
        seed.create_vertex(VId(3), vec![HIDDEN], vec![]);
        seed.add_edge(EId(11), VId(1), VId(3), vec![]);
    }
    db.write(cx, seed).await.unwrap();
    if hidden == 1 {
        let mut other = WriteBatch::new(H);
        other.add_edge(EId(20), VId(2), VId(1), vec![]);
        db.write(cx, other).await.unwrap();
    }
    db
}

/// FG-INV-20 for creation (fgdb-4iiho item c). It moved here from the
/// authorized WriteBatch laws with fgdb-hxgm1: authorized creation now happens
/// only on these allocating surfaces. Creating an edge between two VISIBLE
/// vertices has one outcome and one exact MaxWork threshold, whatever the
/// capability cannot see around them (the six creation_fixture variants).
#[test]
fn matched_creation_work_threshold_ignores_hidden_data() {
    const CEILING: u64 = 100_000;
    lab(0xa943, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&read_write_grant(), NOW).unwrap();
        let insertion = insert("MATCH (a:Visible)-[:R]->(b:Visible) INSERT (a)-[:S]->(b)");
        let mut thresholds = Vec::new();
        for variant in 0..=5_u8 {
            let (mut low, mut high) = (0_u64, CEILING);
            while low < high {
                let middle = low + (high - low) / 2;
                let limited = token.attenuate(Restriction::MaxWork(middle)).unwrap();
                let mut db = creation_fixture(&commit, variant).await;
                match db
                    .execute_graph_insert_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &limited,
                        "main",
                        &insertion,
                        policy(),
                        || NOW,
                    )
                    .await
                {
                    Ok((stats, _)) => {
                        assert_eq!(
                            (stats.created_vertices, stats.created_edges),
                            (0, 1),
                            "variant {variant}"
                        );
                        high = middle;
                    }
                    Err(error) => {
                        assert_eq!(
                            authorization(error),
                            Error::LimitExceeded(LimitDimension::Work),
                            "variant {variant} at {middle}"
                        );
                        low = middle + 1;
                    }
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
            assert!(
                low < CEILING,
                "variant {variant}: no threshold below the ceiling"
            );
            thresholds.push(low);
        }
        assert!(
            thresholds
                .iter()
                .all(|threshold| *threshold == thresholds[0]),
            "the creation threshold moved with hidden data: {thresholds:?}"
        );
    });
}

// Same visible graph and bag multiplicity, with optional hidden rows, fields,
// relations and incidence. ID/frontier metadata is not a noninterference claim.
async fn matched_fixture(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    let mut properties = vec![(P, CanonicalScalar::Int(10))];
    if hidden {
        properties.push((SECRET, CanonicalScalar::Int(99)));
    }
    seed.create_vertex(VId(1), vec![L], properties);
    seed.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(20))]);
    seed.add_edge(EId(11), VId(1), VId(2), vec![]);
    seed.add_edge(EId(12), VId(1), VId(2), vec![]);
    if hidden {
        seed.create_vertex(VId(30), vec![HIDDEN], vec![(P, CanonicalScalar::Int(999))]);
        seed.add_edge(EId(31), VId(1), VId(30), vec![]);
    }
    db.write(cx, seed).await.unwrap();
    if hidden {
        let mut other = WriteBatch::new(H);
        other.add_edge(EId(40), VId(1), VId(2), vec![]);
        db.write(cx, other).await.unwrap();
    }
    db
}

fn matched_insert() -> PreparedGraphInsert {
    insert(
        "MATCH (a:Visible)-[:R]->(b:Visible) \
        INSERT (copy:Visible {p:CASE WHEN a.secret IS NULL THEN a.p ELSE 999 END}), \
        (a)-[:S]->(copy), (copy)-[:R]->(b)",
    )
}

#[test]
fn match_insert_uses_masked_properties_and_preserves_occurrence_multiplicity() {
    lab(0xa941, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&read_write_grant(), NOW).unwrap();
        let insertion = matched_insert();
        let mut stats_seen = Vec::new();
        for hidden in [false, true] {
            let mut db = matched_fixture(&commit, hidden).await;
            let frontier = db.frontier().unwrap();
            let original = db.vertex(VId(1)).unwrap();
            let (stats, vertices, edges, completion) = db
                .execute_graph_insert_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &insertion,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (2, 4));
            assert_eq!(stats.selection.result_rows, 2);
            assert_eq!((vertices.len(), edges.len()), (2, 4));
            let seq = db.frontier().unwrap();
            assert_eq!(seq.0, frontier.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq }
            );
            assert_eq!(db.vertex(VId(1)).unwrap(), original);
            for (occurrence, vertex) in vertices.iter().copied().enumerate() {
                assert_eq!(
                    db.vertex(vertex).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(10))]
                );
                let first = db.edge(edges[2 * occurrence]).unwrap().unwrap();
                let second = db.edge(edges[2 * occurrence + 1]).unwrap().unwrap();
                assert_eq!(
                    (first.entry.src, first.entry.relation, first.entry.dst),
                    (VId(1), S, vertex)
                );
                assert_eq!(
                    (second.entry.src, second.entry.relation, second.entry.dst),
                    (vertex, R, VId(2))
                );
            }
            assert_eq!(txn.outstanding_obligations(), 0);
            stats_seen.push(stats);
        }
        assert_eq!(
            stats_seen[0], stats_seen[1],
            "hidden data changed visible execution statistics"
        );
    });
}

#[test]
fn empty_scoped_match_closes_without_publication_or_identity_reservations() {
    lab(0xa942, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority
            .issue_at(&read_write_grant(), NOW)
            .unwrap()
            .attenuate(Restriction::MaxRows(0))
            .unwrap();
        let mut db = matched_fixture(&commit, true).await;
        let vertex_request = GraphInsertRequest::Vertex { row: 0, vertex: 0 };
        let edge_request = GraphInsertRequest::Edge { row: 0, edge: 0 };
        let ElementId::Vertex(before_vertex) =
            db.allocate_identity(&query, vertex_request).unwrap()
        else {
            panic!("vertex allocator kind")
        };
        let ElementId::Edge(before_edge) = db.allocate_identity(&query, edge_request).unwrap()
        else {
            panic!("edge allocator kind")
        };
        let frontier = db.frontier().unwrap();
        let original = db.vertices().unwrap();
        let insertion = insert("MATCH (a:Hidden) INSERT (copy:Visible), (a)-[:S]->(copy)");
        let (stats, vertices, edges, completion) = db
            .execute_graph_insert_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &insertion,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
        assert!(vertices.is_empty() && edges.is_empty());
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::ReadClosed {
                snapshot_seq: frontier,
                validated_through: frontier
            }
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap(), original);
        assert_eq!(
            db.allocate_identity(&query, vertex_request).unwrap(),
            ElementId::Vertex(VId(before_vertex.0 + 1))
        );
        assert_eq!(
            db.allocate_identity(&query, edge_request).unwrap(),
            ElementId::Edge(EId(before_edge.0 + 1))
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn write_only_authority_cannot_gain_selection_reads_even_for_an_empty_match() {
    lab(0xa943, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority
            .issue_at(&grant(), NOW)
            .unwrap()
            .attenuate(Restriction::MaxWork(0))
            .unwrap();
        for text in [
            "MATCH (a:Visible) INSERT (copy:Visible {p:a.p})",
            "MATCH (a:Hidden) INSERT (copy:Visible)",
        ] {
            let mut db = matched_fixture(&commit, true).await;
            let frontier = db.frontier().unwrap();
            let original = db.vertices().unwrap();
            let insertion = insert(text);
            let error = db
                .execute_graph_insert_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &insertion,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(error), Error::PermissionDenied);
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(db.vertices().unwrap(), original);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn hidden_data_changes_neither_match_insert_work_nor_node_refusal_thresholds() {
    lab(0xa944, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&read_write_grant(), NOW).unwrap();
        let insertion = matched_insert();
        for dimension in [LimitDimension::Work, LimitDimension::Nodes] {
            let mut floors = Vec::new();
            for hidden in [false, true] {
                let (mut low, mut high) = (0_u64, 4096_u64);
                while low < high {
                    let middle = low + (high - low) / 2;
                    let restriction = match dimension {
                        LimitDimension::Work => Restriction::MaxWork(middle),
                        LimitDimension::Nodes => Restriction::MaxNodes(middle),
                        _ => unreachable!(),
                    };
                    let limited = token.attenuate(restriction).unwrap();
                    let mut db = matched_fixture(&commit, hidden).await;
                    let frontier = db.frontier().unwrap();
                    let original = db.vertices().unwrap();
                    match db
                        .execute_graph_insert_authorized(
                            &txn,
                            &query,
                            &commit,
                            &authority,
                            &limited,
                            "main",
                            &insertion,
                            policy(),
                            || NOW,
                        )
                        .await
                    {
                        Ok((stats, _)) => {
                            assert_eq!((stats.created_vertices, stats.created_edges), (2, 4));
                            high = middle;
                        }
                        Err(error) => {
                            assert_eq!(authorization(error), Error::LimitExceeded(dimension));
                            assert_eq!(db.frontier().unwrap(), frontier);
                            assert_eq!(db.vertices().unwrap(), original);
                            low = middle + 1;
                        }
                    }
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
                assert!(low > 0 && low < 4096);
                floors.push(low);
            }
            assert_eq!(
                floors[0], floors[1],
                "{dimension:?}: hidden data changed the refusal threshold"
            );
        }
    });
}

/// The native staging write_ordered_authorized ran before fgdb-hxgm1: the
/// same permit, workspace, per-intent authorization and completion, without
/// the chosen-identity refusal that path now applies first (which charges
/// nothing). It is only the independent allowance witness below; no client
/// surface reaches it.
async fn native_ordered_witness(
    db: &mut Database<MemVfs>,
    txn_cx: &TxnCx,
    commit_cx: &CommitCx,
    authority: &Authority,
    token: &CapabilityToken,
    batches: Vec<WriteBatch>,
) -> Result<CommitSeq, WriteTxnError> {
    let verified = authority
        .verify_at(token, "main", NOW)
        .map_err(WriteTxnError::Authorization)?;
    let permit = verified
        .begin_write_at("main", NOW)
        .map_err(WriteTxnError::Authorization)?;
    commit_cx
        .with_restriction_async(async {
            let mut execution = Execution {
                cx: commit_cx,
                permit,
                clock: || NOW,
            };
            execution.checkpoint()?;
            let mut workspace = Workspace(Some(db.begin(txn_cx)?));
            for batch in batches {
                for row in batch.rows {
                    execution.checkpoint()?;
                    stage(
                        workspace.transaction(),
                        db,
                        batch.relation,
                        row,
                        &mut execution,
                    )?;
                }
            }
            match workspace
                .transaction()
                .complete_controlled(db, commit_cx, None, true, || execution.checkpoint())
                .await?
            {
                EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Ok(commit_seq),
                other => panic!("witness completion: {other:?}"),
            }
        })
        .await
}

#[test]
fn selection_and_write_share_one_node_allowance_not_two_independent_permits() {
    lab(0xa945, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&read_write_grant(), NOW).unwrap();
        let insertion = matched_insert();
        let mut floors = Vec::new();
        for matched in [false, true] {
            let (mut low, mut high) = (0_u64, 128_u64);
            while low < high {
                let middle = low + (high - low) / 2;
                let limited = token.attenuate(Restriction::MaxNodes(middle)).unwrap();
                let mut db = matched_fixture(&commit, false).await;
                let success = if matched {
                    match db
                        .execute_graph_insert_authorized(
                            &txn,
                            &query,
                            &commit,
                            &authority,
                            &limited,
                            "main",
                            &insertion,
                            policy(),
                            || NOW,
                        )
                        .await
                    {
                        Ok(_) => true,
                        Err(error) => {
                            assert_eq!(
                                authorization(error),
                                Error::LimitExceeded(LimitDimension::Nodes)
                            );
                            false
                        }
                    }
                } else {
                    // Independent native-write witness for exactly the effects
                    // selected above, in the collector's occurrence order.
                    let mut batches = Vec::new();
                    for occurrence in 0..2_u128 {
                        let vertex = VId(100 + occurrence);
                        let mut create = WriteBatch::new(R);
                        create.create_vertex(vertex, vec![L], vec![(P, CanonicalScalar::Int(10))]);
                        let mut incoming = WriteBatch::new(S);
                        incoming.add_edge(EId(100 + 2 * occurrence), VId(1), vertex, vec![]);
                        let mut outgoing = WriteBatch::new(R);
                        outgoing.add_edge(EId(101 + 2 * occurrence), vertex, VId(2), vec![]);
                        batches.extend([create, incoming, outgoing]);
                    }
                    match native_ordered_witness(
                        &mut db, &txn, &commit, &authority, &limited, batches,
                    )
                    .await
                    {
                        Ok(_) => true,
                        Err(WriteTxnError::Authorization(error)) => {
                            assert_eq!(error, Error::LimitExceeded(LimitDimension::Nodes));
                            false
                        }
                        other => panic!("native allowance witness: {other:?}"),
                    }
                };
                if success {
                    high = middle;
                } else {
                    low = middle + 1;
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
            assert!(low > 2 && low < 128);
            floors.push(low);
        }
        // The source admits the two visible vertices once, in addition to all
        // the same native before/after admissions. Reissuing a write permit
        // after selection drops these two units and makes this assertion fail.
        assert_eq!(floors[1], floors[0] + 2);
    });
}

#[test]
fn expiry_during_match_collection_or_final_admission_never_publishes_a_prefix() {
    lab(0xa946, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&read_write_grant(), NOW).unwrap();
        let insertion = matched_insert();
        let mut samples = 0_usize;
        let mut db = matched_fixture(&commit, false).await;
        db.execute_graph_insert_authorized(
            &txn,
            &query,
            &commit,
            &authority,
            &token,
            "main",
            &insertion,
            policy(),
            || {
                samples += 1;
                NOW
            },
        )
        .await
        .unwrap();
        let total = samples;
        assert!(total > 3);
        for cutoff in [1, total / 2, total] {
            let mut db = matched_fixture(&commit, false).await;
            let frontier = db.frontier().unwrap();
            let vertices = db.vertices().unwrap();
            let edges = db.edges().unwrap();
            let mut calls = 0;
            let error = db
                .execute_graph_insert_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &insertion,
                    policy(),
                    || {
                        calls += 1;
                        if calls >= cutoff { 10_000 } else { NOW }
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(error), Error::Expired);
            assert_eq!(db.frontier().unwrap(), frontier, "cutoff={cutoff}");
            assert_eq!(db.vertices().unwrap(), vertices);
            assert_eq!(db.edges().unwrap(), edges);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn insertion_query_bills_final_rows_and_pages_without_suppressing_creations() {
    lab(0xa951, |contexts| async move {
        use fgdb_gql::algebra::{GraphValue, GraphValueOrder};

        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        for output_rows in [0, 1] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let before = db.frontier().unwrap();
            let query = projected_insert(
                insert(
                    "UNWIND [3,1,3] AS p CREATE (a:Visible {p:p}), \
                     (b:Visible), (a)-[:R]->(b)",
                ),
                vec![GraphInsertBinding::VertexProperty { vertex: 0, key: P }],
                GraphSetQuantifier::Distinct,
            )
            .with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap()
            .with_page(1, Some(output_rows));
            let token = authority
                .issue_at(&read_write_grant(), NOW)
                .unwrap()
                .attenuate(Restriction::MaxRows(output_rows))
                .unwrap();
            let mut bounded = policy();
            bounded.query = GqlQueryPolicy::new(0, output_rows, 1_000_000, 100_000);
            let (stats, result, completion) = db
                .execute_graph_insert_query_authorized(
                    &txn,
                    &query_cx,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    bounded,
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (6, 3));
            assert_eq!(stats.selection.result_rows, 3);
            assert_eq!(result.rows.snapshot_records, 0);
            assert_eq!(result.rows.result_rows, output_rows);
            assert!(result.evaluator.work_units >= stats.evaluator.work_units);
            let expected = if output_rows == 0 {
                vec![]
            } else {
                vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                    CanonicalScalar::Int(3),
                )])]
            };
            assert_eq!(result.value, expected);
            let seq = db.frontier().unwrap();
            assert_eq!(seq.0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq }
            );
            let vertices = db.vertices().unwrap();
            let edges = db.edges().unwrap();
            assert_eq!((vertices.len(), edges.len()), (6, 3));
            let properties: Vec<_> = vertices.iter().filter_map(|v| v.props.first()).collect();
            assert_eq!(
                properties,
                vec![
                    &(P, CanonicalScalar::Int(3)),
                    &(P, CanonicalScalar::Int(1)),
                    &(P, CanonicalScalar::Int(3)),
                ]
            );
            db.compact(&commit).await.unwrap();
            drop(db);
            let db = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(
                (db.vertices().unwrap(), db.edges().unwrap()),
                (vertices, edges)
            );
            assert_eq!(db.frontier().unwrap(), seq);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn insertion_query_requires_readwrite_before_identity_allocation_even_with_limit_zero() {
    lab(0xa952, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let query = projected_insert(
            insert("CREATE (n:Visible {p:7})"),
            vec![GraphInsertBinding::CreatedVertex(0)],
            GraphSetQuantifier::All,
        )
        .with_page(0, Some(0));
        for rights in [Rights::Read, Rights::Write] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut denied = grant();
            denied.rights = rights;
            denied.limits.max_work = 0;
            let token = authority.issue_at(&denied, NOW).unwrap();
            let error = db
                .execute_graph_insert_query_authorized(
                    &txn,
                    &query_cx,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(query_authorization(&error), Some(Error::PermissionDenied));
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(
                db.allocate_identity(&query_cx, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                    .unwrap(),
                ElementId::Vertex(VId(1))
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn insertion_query_projection_quota_and_creation_scope_failures_publish_nothing() {
    lab(0xa953, |contexts| async move {
        use fgdb_gql::{
            GraphIntegerBinary, GraphIntegerErrorKind, GraphIntegerExpression, GraphIntegerOp,
            GraphSetExecutionError,
        };

        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&read_write_grant(), NOW).unwrap();
        for mode in 0..4 {
            let mut db = matched_fixture(&commit, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let insertion = if mode == 3 {
                insert("CREATE (a:Visible {p:2}), (b:Visible {secret:1})")
            } else {
                insert("UNWIND [2,1] AS p CREATE (a:Visible {p:p})")
            };
            let bindings = vec![GraphInsertBinding::VertexProperty { vertex: 0, key: P }];
            let query = if mode == 0 {
                PreparedGraphInsertQuery::prepare(
                    insertion,
                    bindings,
                    vec![GraphSetProjection::new(
                        "value",
                        GraphSetValue::Integer(
                            GraphIntegerExpression::prepare(&[
                                GraphIntegerOp::Column(0),
                                GraphIntegerOp::Column(0),
                                GraphIntegerOp::Literal(Some(1)),
                                GraphIntegerOp::Binary(GraphIntegerBinary::Subtract),
                                GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
                            ])
                            .unwrap(),
                        ),
                    )],
                    GraphSetQuantifier::All,
                )
                .unwrap()
            } else {
                projected_insert(insertion, bindings, GraphSetQuantifier::All)
            };
            let query = if mode == 3 {
                // An empty result must never conceal a forbidden creation.
                query.with_page(0, Some(0))
            } else {
                query
            };
            let token = if mode == 1 {
                token.attenuate(Restriction::MaxRows(1)).unwrap()
            } else {
                token.clone()
            };
            let mut bounded = policy();
            if mode == 2 {
                bounded.query = GqlQueryPolicy::new(0, 1, 1_000_000, 100_000);
            }
            let error = db
                .execute_graph_insert_query_authorized(
                    &txn,
                    &query_cx,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    bounded,
                    || NOW,
                )
                .await
                .unwrap_err();
            match mode {
                0 => assert!(
                    matches!(
                        &error,
                        GqlQueryError::Source(GraphInsertQueryError::Returning(
                            GraphSetExecutionError::Projection {
                                row: 1,
                                column: 0,
                                error: fgdb_gql::GraphIntegerError {
                                    kind: GraphIntegerErrorKind::DivisionByZero,
                                    ..
                                },
                            }
                        ))
                    ),
                    "{error:?}"
                ),
                1 => assert_eq!(
                    query_authorization(&error),
                    Some(Error::LimitExceeded(LimitDimension::Rows))
                ),
                2 => assert!(matches!(&error, GqlQueryError::Rows(_)), "{error:?}"),
                3 => assert_eq!(query_authorization(&error), Some(Error::ScopeDenied)),
                _ => unreachable!(),
            }
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
                "mode={mode}"
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn insertion_query_masks_source_values_and_keeps_parallel_edge_occurrences() {
    lab(0xa954, |contexts| async move {
        use fgdb_gql::algebra::GraphValue;
        use fgdb_gql::insertion::GraphInsertVertex;
        use fgdb_gql::{GraphMutationValue, PreparedGraphSetText};

        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&read_write_grant(), NOW).unwrap();
        let source = PreparedGraphSetText::prepare(
            "MATCH (a:Visible)-[:R]->(b:Visible) RETURN a.p AS p, a.secret AS hidden",
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let query = projected_insert(
            PreparedGraphInsert::prepare_relation(
                source,
                R,
                vec![GraphInsertVertex {
                    labels: vec![L],
                    properties: vec![(P, GraphMutationValue::Column(0))],
                }],
                vec![],
            )
            .unwrap(),
            vec![
                GraphInsertBinding::Input(0),
                GraphInsertBinding::Input(1),
                GraphInsertBinding::VertexProperty { vertex: 0, key: P },
            ],
            GraphSetQuantifier::All,
        );
        let mut results = Vec::new();
        for hidden in [false, true] {
            let mut db = matched_fixture(&commit, hidden).await;
            let before = db.frontier().unwrap();
            let (stats, result, completion) = db
                .execute_graph_insert_query_authorized(
                    &txn,
                    &query_cx,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(stats.created_vertices, 2);
            assert_eq!(
                result.value,
                vec![
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Scalar(CanonicalScalar::Int(10)),
                        GraphValue::Scalar(CanonicalScalar::Null),
                        GraphValue::Scalar(CanonicalScalar::Int(10)),
                    ]);
                    2
                ]
            );
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            results.push((stats, result));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        assert_eq!(
            results[0], results[1],
            "hidden records changed public values or usage"
        );
    });
}

#[test]
fn insertion_query_expiry_through_final_admission_aborts_all_effects() {
    lab(0xa955, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&read_write_grant(), NOW).unwrap();
        let query = projected_insert(
            insert("UNWIND [2,1] AS p CREATE (a:Visible {p:p}), (b:Visible), (a)-[:S]->(b)"),
            vec![
                GraphInsertBinding::CreatedVertex(0),
                GraphInsertBinding::CreatedEdge(0),
            ],
            GraphSetQuantifier::All,
        );
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut total = 0;
        db.execute_graph_insert_query_authorized(
            &txn,
            &query_cx,
            &commit,
            &authority,
            &token,
            "main",
            &query,
            policy(),
            || {
                total += 1;
                NOW
            },
        )
        .await
        .unwrap();
        assert!(total > 3);
        for cutoff in [1, total / 3, 2 * total / 3, total] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut calls = 0;
            let error = db
                .execute_graph_insert_query_authorized(
                    &txn,
                    &query_cx,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(),
                    || {
                        calls += 1;
                        if calls >= cutoff { 10_000 } else { NOW }
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(
                query_authorization(&error),
                Some(Error::Expired),
                "cutoff={cutoff}"
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn insertion_query_cancellation_discards_private_results_and_releases_workspace() {
    let ((), report) = run_async_under_lab(0xa956, |root| async move {
        let query = || {
            projected_insert(
                insert("UNWIND [2,1] AS p CREATE (a:Visible {p:p}), (b:Visible), (a)-[:R]->(b)"),
                vec![GraphInsertBinding::CreatedVertex(0)],
                GraphSetQuantifier::All,
            )
        };
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&read_write_grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut total = 0;
        db.execute_graph_insert_query_authorized(
            &txn,
            &query_cx,
            &commit,
            &issuer,
            &token,
            "main",
            &query(),
            policy(),
            || {
                total += 1;
                NOW
            },
        )
        .await
        .unwrap();
        assert!(total > 3);
        for cutoff in [1, total / 2] {
            let mut handle = root
                .spawn(move |child| async move {
                    let contexts = PurposeContexts::narrow_runtime_root(&child);
                    let commit = contexts.commit();
                    let query_cx = contexts.query();
                    let txn = contexts.txn();
                    let authority = authority();
                    let token = authority.issue_at(&read_write_grant(), NOW).unwrap();
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    let before = db.frontier().unwrap();
                    let mut calls = 0;
                    let error = db
                        .execute_graph_insert_query_authorized(
                            &txn,
                            &query_cx,
                            &commit,
                            &authority,
                            &token,
                            "main",
                            &query(),
                            policy(),
                            || {
                                calls += 1;
                                if calls == cutoff {
                                    child.cancel_with(
                                        asupersync::CancelKind::User,
                                        Some("insertion RETURN cancellation"),
                                    );
                                }
                                NOW
                            },
                        )
                        .await
                        .unwrap_err();
                    assert!(matches!(
                        error,
                        GqlQueryError::Interrupted(WriteTxnError::Interrupted(_))
                            | GqlQueryError::Source(GraphInsertQueryError::Insertion(
                                GraphInsertError::Source(WriteTxnError::Interrupted(_))
                            ))
                    ));
                    assert_eq!(db.frontier().unwrap(), before);
                    assert!(db.vertices().unwrap().is_empty());
                    assert!(db.edges().unwrap().is_empty());
                    assert_eq!(txn.outstanding_obligations(), 0);
                })
                .expect("insertion RETURN cancellation child starts");
            assert_eq!(handle.join(&root).await, Ok(()));
        }
        assert!(root.checkpoint().is_ok(), "the supervisor remains live");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
