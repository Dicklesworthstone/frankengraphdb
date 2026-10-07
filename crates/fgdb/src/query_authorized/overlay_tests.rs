//! Compare source ownership strategies, not a second invocation of the same
//! sparse implementation. Both feed the existing masking and GLA body. Trace
//! every charged callback; physical-history polls deliberately remain unbilled.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch, WriteTxn};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error, Grant, LimitDimension, QueryLimits, Rights, Scope};
use std::cell::RefCell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xc8; 32]);
const L: LabelId = LabelId(1);
const H: LabelId = LabelId(9);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc7; 32], NS, [0xc9; 32])
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 1_000_000)
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0xa720), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        properties: Scope::only([P]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows: 100_000,
        },
        expires_at_ms: 10_000,
    }
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .with_duplicates()
}

async fn seed(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, value) in [(10, 10), (20, 20)] {
        let mut props = vec![(P, CanonicalScalar::Int(value))];
        let mut labels = vec![L];
        if hidden {
            props.push((SECRET, CanonicalScalar::Int(900)));
            labels.push(H);
        }
        batch.create_vertex(VId(id), labels, props);
    }
    for id in [10, 11] {
        let mut props = vec![(P, CanonicalScalar::Int(30))];
        if hidden {
            props.push((SECRET, CanonicalScalar::Int(901)));
        }
        batch.add_edge(EId(id), VId(10), VId(20), props);
    }
    if hidden {
        batch.create_vertex(VId(99), vec![H], vec![(P, CanonicalScalar::Int(902))]);
        batch.add_edge(EId(99), VId(10), VId(99), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut other = WriteBatch::new(S);
        other.add_edge(EId(100), VId(10), VId(20), vec![]);
        db.write(cx, other).await.unwrap();
    }
    db
}

#[derive(Default)]
struct Trace {
    events: RefCell<Vec<u8>>,
    stop: Option<usize>,
}
impl Trace {
    fn charge(&self, event: u8) -> Result<(), QueryError> {
        let mut events = self.events.borrow_mut();
        events.push(event);
        if self.stop == Some(events.len()) {
            Err(QueryError::Authorization(Error::LimitExceeded(
                LimitDimension::Work,
            )))
        } else {
            Ok(())
        }
    }
}

// Incumbent: materialize the full native vertex/edge collections, then admit.
// Its ownership and merge mechanism differ from OverlayRows. Keeping it live
// here makes an omitted tombstone, misplaced mask or charge order observable.
fn incumbent(
    database: &Database<MemVfs>,
    transaction: &WriteTxn,
    cx: &QueryCx,
    pattern: &PreparedGraphPattern<GraphValueRow>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    trace: &Trace,
) -> Governed<GraphValueRow> {
    let vertices = transaction.vertices(database).unwrap();
    let edges = if pattern.plan().reads_edges() {
        transaction.edges(database).unwrap()
    } else {
        Vec::new()
    };
    trace.charge(1).map_err(GqlQueryError::Interrupted)?;
    database.ensure_readable().map_err(GqlQueryError::Source)?;
    cx.with_restriction(|| {
        let mut usage = AdmissionUsage::default();
        let mut tables = Tables::new();
        {
            let mut control = |event| {
                trace.charge(1).map_err(GqlQueryError::Interrupted)?;
                usage.observe::<ReadError, QueryError>(policy, event)
            };
            for row in &vertices {
                tables.admit_vertex(
                    row,
                    scope,
                    &mut || trace.charge(2).map_err(GqlQueryError::Interrupted),
                    &mut control,
                )?;
            }
            for row in &edges {
                let edge = &row.entry;
                tables.admit_edge(
                    ((edge.eid, edge.src, edge.relation, edge.dst), &row.props),
                    scope,
                    &mut control,
                )?;
            }
            tables.metadata(pattern.plan(), scope, &mut control)?;
        }
        execute_tables(tables, pattern, scope, policy, usage, &mut || {
            trace.charge(1)
        })
    })
}

fn sparse(
    database: &Database<MemVfs>,
    transaction: &WriteTxn,
    cx: &QueryCx,
    pattern: &PreparedGraphPattern<GraphValueRow>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    trace: &Trace,
) -> Governed<GraphValueRow> {
    let owner = OverlayRows::new(
        transaction,
        database,
        pattern.plan().reads_edges(),
        &mut || Ok(()),
    )
    .unwrap();
    database.select_for_authorized_overlay(
        cx,
        &owner,
        pattern,
        scope,
        policy,
        || trace.charge(2),
        || Ok(()),
        || trace.charge(1),
    )
}

fn equal_results(left: Governed<GraphValueRow>, right: Governed<GraphValueRow>) {
    match (left, right) {
        (Ok(left), Ok(right)) => {
            assert_eq!(left.value, right.value);
            assert_eq!(left.rows, right.rows);
            assert_eq!(left.evaluator, right.evaluator);
        }
        (Err(left), Err(right)) => assert_eq!(format!("{left:?}"), format!("{right:?}")),
        (left, right) => panic!("source strategies differ: {left:?} / {right:?}"),
    }
}

#[test]
fn sparse_authorized_admission_preserves_all_charged_callbacks_and_native_thresholds() {
    let ((), report) = run_async_under_lab(0xa720, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let verified = authority.verify_at(&token, "main", 100).unwrap();
        let scope = verified.predicates();
        for hide_successor in [false, true] {
            for text in [
                "MATCH (a:L) RETURN a, a.p, a.secret",
                "MATCH (a:L)-[e:R]->(b:L) RETURN a, b, e.p, a.secret",
                "MATCH (a:L)-[e]->(b:L) RETURN a, b, e.p, a.secret",
            ] {
                let pattern = pattern(text);
                let mut visible = None;
                for hidden in [false, true] {
                    let mut db = seed(&commit, hidden).await;
                    let mut transaction = db.begin(&txcx).unwrap();
                    let mut batch = WriteBatch::new(R);
                    batch.create_vertex(VId(0), vec![L], vec![(P, CanonicalScalar::Int(0))]);
                    batch.add_edge(EId(0), VId(0), VId(10), vec![(P, CanonicalScalar::Int(1))]);
                    batch.set_vertex_property(VId(20), P, Some(CanonicalScalar::Int(21)));
                    if hide_successor {
                        batch.set_vertex_label(VId(20), L, false);
                    }
                    transaction.write(&mut db, batch).unwrap();
                    // A native fixture may stage a scope escape; source masking
                    // must hide its successor rather than revive its old row.
                    let trace = Trace::default();
                    let result =
                        sparse(&db, &transaction, &cx, &pattern, scope, policy(), &trace).unwrap();
                    let charged = trace.events.into_inner();
                    let old_trace = Trace::default();
                    let old = incumbent(
                        &db,
                        &transaction,
                        &cx,
                        &pattern,
                        scope,
                        policy(),
                        &old_trace,
                    );
                    let signature = (
                        result.value.clone(),
                        result.rows,
                        result.evaluator,
                        charged.clone(),
                    );
                    equal_results(Ok(result), old);
                    assert_eq!(old_trace.events.into_inner(), charged);
                    if hidden {
                        assert_eq!(
                            Some(signature.clone()),
                            visible,
                            "hidden graph changed visible rows, native stats or a charged callback"
                        );
                    } else {
                        visible = Some(signature.clone());
                    }
                    for stop in 1..=charged.len() {
                        let left = Trace {
                            stop: Some(stop),
                            ..Trace::default()
                        };
                        let right = Trace {
                            stop: Some(stop),
                            ..Trace::default()
                        };
                        let new = sparse(&db, &transaction, &cx, &pattern, scope, policy(), &left);
                        let old =
                            incumbent(&db, &transaction, &cx, &pattern, scope, policy(), &right);
                        assert!(matches!(new, Err(GqlQueryError::Interrupted(_))));
                        equal_results(new, old);
                        assert_eq!(*left.events.borrow(), charged[..stop]);
                        assert_eq!(*left.events.borrow(), *right.events.borrow());
                    }
                    let (_, rows, evaluator, _) = signature;
                    let exact = GqlQueryPolicy::new(
                        rows.snapshot_records,
                        rows.result_rows,
                        evaluator.work_units,
                        evaluator.scratch_entries,
                    );
                    for below in [None, Some(0), Some(1), Some(2), Some(3)] {
                        let mut limits = [
                            rows.snapshot_records,
                            rows.result_rows,
                            evaluator.work_units,
                            evaluator.scratch_entries,
                        ];
                        if let Some(dimension) = below {
                            if limits[dimension] == 0 {
                                continue;
                            }
                            limits[dimension] -= 1;
                        }
                        let budget = if below.is_none() {
                            exact
                        } else {
                            GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3])
                        };
                        let left = Trace::default();
                        let right = Trace::default();
                        let new = sparse(&db, &transaction, &cx, &pattern, scope, budget, &left);
                        let old =
                            incumbent(&db, &transaction, &cx, &pattern, scope, budget, &right);
                        assert_eq!(new.is_ok(), below.is_none());
                        equal_results(new, old);
                        assert_eq!(*left.events.borrow(), *right.events.borrow());
                    }
                    transaction.abort();
                    assert_eq!(txcx.outstanding_obligations(), 0);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// The edge-rooted query binds no vertex by scan (every vertex it reads is an
/// endpoint of an `:R` edge); the vertex query's scan is labelled `:L`.
const EDGE_QUERY: &str = "MATCH (a:L)-[e:R]->(b:L) RETURN e.p";
const VERTEX_QUERY: &str = "MATCH (a:L) RETURN a.p";

/// Runs one authorized query inside a transaction staging a disjoint write,
/// lets `concurrent` commit first, then returns the transaction's commit
/// outcome. The seed has 10 -[:R]-> 20, both `:L`, and an unlabelled `:S`
/// neighbourhood 30 -[:S]-> 40 that neither query can bind.
async fn authorized_query_after(
    contexts: &PurposeContexts,
    query: &str,
    concurrent: WriteBatch,
) -> Result<fgdb_types::CommitSeq, crate::WriteTxnError> {
    let cx = contexts.query();
    let commit = contexts.commit();
    let txcx = contexts.txn();
    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, labels) in [(10, vec![L]), (20, vec![L]), (30, vec![]), (40, vec![])] {
        batch.create_vertex(VId(id), labels, vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    batch.add_edge(EId(1), VId(10), VId(20), vec![(P, CanonicalScalar::Int(1))]);
    db.write(&commit, batch).await.unwrap();
    let mut other = WriteBatch::new(S);
    other.add_edge(EId(2), VId(30), VId(40), vec![(P, CanonicalScalar::Int(2))]);
    db.write(&commit, other).await.unwrap();
    let authority = authority();
    let token = authority.issue_at(&grant(), 100).unwrap();
    let verified = authority.verify_at(&token, "main", 100).unwrap();
    let mut transaction = db.begin(&txcx).unwrap();
    {
        let owner = OverlayRows::new(&transaction, &db, true, &mut || Ok(())).unwrap();
        let rows = db
            .select_for_authorized_overlay(
                &cx,
                &owner,
                &pattern(query),
                verified.predicates(),
                policy(),
                || Ok(()),
                || Ok(()),
                || Ok(()),
            )
            .unwrap();
        let expected = if query == EDGE_QUERY { 1 } else { 2 };
        assert_eq!(rows.value.len(), expected, "{query}");
    }
    let mut staged = WriteBatch::new(R);
    staged.create_vertex(VId(50), vec![L], vec![]);
    transaction.write(&mut db, staged).unwrap();
    let mut writer = db.begin(&txcx).unwrap();
    writer.write(&mut db, concurrent).unwrap();
    writer.commit(&mut db, &commit).await.unwrap();
    transaction.commit(&mut db, &commit).await
}

/// fgdb-h1d6l. Neither query can bind the unlabelled `:S` neighbourhood, so
/// a concurrent `:S` edge and a property change on vertex 30 change nothing
/// either read. Before, the authorized vertex scan recorded every vertex and
/// the edge scan every edge, and both aborted on vertex 30.
#[test]
fn authorized_scans_ignore_writes_they_cannot_bind() {
    let ((), report) = run_async_under_lab(0xa722, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for query in [EDGE_QUERY, VERTEX_QUERY] {
            let mut concurrent = WriteBatch::new(S);
            concurrent.add_edge(EId(3), VId(40), VId(30), vec![]);
            concurrent.set_vertex_property(VId(30), P, Some(CanonicalScalar::Int(300)));
            let committed = authorized_query_after(&contexts, query, concurrent).await;
            assert!(committed.is_ok(), "{query}: false conflict {committed:?}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Soundness guards for the narrowed witnesses, one per way a row can
/// appear or change: a new `:R` edge (the relation witness), a matched
/// destination leaving `:L` (its endpoint read), an outsider joining `:L`
/// (the label witness) and a matched vertex's property (its read).
#[test]
fn authorized_scans_still_abort_on_changes_they_can_bind() {
    let ((), report) = run_async_under_lab(0xa723, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut phantom = WriteBatch::new(R);
        phantom.add_edge(EId(3), VId(30), VId(40), vec![]);
        let mut destination = WriteBatch::new(R);
        destination.set_vertex_label(VId(20), L, false);
        let mut member = WriteBatch::new(R);
        member.set_vertex_label(VId(30), L, true);
        let mut matched = WriteBatch::new(R);
        matched.set_vertex_property(VId(10), P, Some(CanonicalScalar::Int(100)));
        for (case, query, concurrent) in [
            ("new :R edge", EDGE_QUERY, phantom),
            ("destination leaves :L", EDGE_QUERY, destination),
            ("outsider joins :L", VERTEX_QUERY, member),
            ("matched property", VERTEX_QUERY, matched),
        ] {
            let committed = authorized_query_after(&contexts, query, concurrent).await;
            assert!(
                committed
                    .as_ref()
                    .is_err_and(|error| format!("{error:?}").contains("FG-LAW-FCW-READ-01")),
                "{case}: a row the query read changed; the commit must abort: {committed:?}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn missing_edge_domain_refuses_instead_of_falling_back_to_unstaged_basis() {
    let ((), report) = run_async_under_lab(0xa721, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let mut db = seed(&commit, false).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let verified = authority.verify_at(&token, "main", 100).unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.delete_edge(EId(10));
        transaction.write(&mut db, batch).unwrap();
        {
            let owner = OverlayRows::new(&transaction, &db, false, &mut || Ok(())).unwrap();
            let result = db.select_for_authorized_overlay(
                &cx,
                &owner,
                &pattern("MATCH (a:L)-[e:R]->(b:L) RETURN e.p"),
                verified.predicates(),
                policy(),
                || Ok(()),
                || Ok(()),
                || Ok(()),
            );
            assert!(matches!(
                result,
                Err(GqlQueryError::IdentifiedEdgesRequired)
            ));
        }
        transaction.abort();
        assert!(db.edge(EId(10)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
