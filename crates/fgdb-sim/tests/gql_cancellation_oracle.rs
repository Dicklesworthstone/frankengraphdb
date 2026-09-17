//! Exact-checkpoint interruption over native storage adapters.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
type Vertex = (Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>);
type Edge = (VId, RelationId, VId, Vec<(PropertyKeyId, CanonicalScalar)>);
#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    vertices: BTreeMap<VId, Vertex>,
    edges: BTreeMap<EId, Edge>,
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
async fn seeded(cx: &CommitCx, seed: u64) -> Database<MemVfs> {
    let mut db = Database::open_memory(
        cx,
        DatabaseKeys::new(
            [0x91; 32],
            DatabaseSecurityNamespaceId([0x92; 32]),
            [0x93; 32],
        ),
    )
    .await
    .unwrap();
    let n = 6 + seed % 3;
    let mut batch = WriteBatch::new(R);
    for i in 1..=n {
        batch.create_vertex(
            VId(u128::from(i)),
            vec![PERSON],
            vec![
                (P, CanonicalScalar::Int(i as i64)),
                (Q, CanonicalScalar::Int((seed % 100 + i) as i64)),
            ],
        );
    }
    for i in 1..n - 1 {
        batch.add_edge(
            EId(u128::from(i)),
            VId(u128::from(i)),
            VId(u128::from(i + 1)),
            vec![],
        );
    }
    batch.add_edge(EId(30), VId(1), VId(3 + u128::from(seed % 2)), vec![]);
    db.write(cx, batch).await.unwrap();
    db
}
fn observed(db: &Database<MemVfs>) -> State {
    State {
        vertices: db
            .vertices()
            .unwrap()
            .into_iter()
            .map(|v| (v.vid, (v.labels, v.props)))
            .collect(),
        edges: db
            .edges()
            .unwrap()
            .into_iter()
            .map(|e| {
                (
                    e.entry.eid,
                    (e.entry.src, e.entry.relation, e.entry.dst, e.props),
                )
            })
            .collect(),
    }
}
fn prefix() -> WriteBatch {
    let mut b = WriteBatch::new(R);
    b.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(-999)));
    b
}
fn stops(n: usize, mut seed: u64) -> Vec<usize> {
    assert!(n > 0);
    if n <= 64 {
        return (1..=n).collect();
    }
    let mut ks = BTreeSet::from([1, 2, n - 1, n]);
    while ks.len() < 64 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ks.insert(1 + seed as usize % n);
    }
    ks.into_iter().collect()
}
fn assert_clean(contexts: &PurposeContexts) {
    assert_eq!(contexts.txn().outstanding_obligations(), 0);
    assert_eq!(contexts.commit().outstanding_obligations(), 0);
}
fn check_state(db: &Database<MemVfs>, before: &State, seq: CommitSeq) {
    assert_eq!(
        db.frontier().unwrap(),
        seq,
        "interruption advanced frontier"
    );
    assert_eq!(&observed(db), before, "interruption leaked logical effects");
}
fn check_identity(db: &mut Database<MemVfs>, cx: &QueryCx, before: &State) {
    use fgdb_gql::insertion::GraphInsertRequest;
    let ElementId::Vertex(v) = db
        .allocate_identity(cx, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
        .unwrap()
    else {
        panic!("vertex allocator returned edge")
    };
    assert!(!before.vertices.contains_key(&v));
    let ElementId::Edge(e) = db
        .allocate_identity(cx, GraphInsertRequest::Edge { row: 0, edge: 0 })
        .unwrap()
    else {
        panic!("edge allocator returned vertex")
    };
    assert!(!before.edges.contains_key(&e));
}
#[test]
fn native_checkpoint_probe_reaches_real_read_and_preserves_context() {
    let ((), report) = run_async_under_lab(0xc001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seeded(&contexts.commit(), 1).await;
        let text = "MATCH (n:Person) RETURN n.p";
        let expected = db
            .query(&cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(Some(2)));
        let injected = cx.with_checkpoint_probe(probe.clone());
        let result = db.query(&injected, text, &GqlParameters::new(), symbols, policy());
        assert!(
            matches!(
                result,
                Err(fgdb::QueryError::Pattern(
                    fgdb_gql::GqlQueryError::Interrupted(_)
                ))
            ),
            "{result:?}"
        );
        assert_eq!(probe.calls(), 2);
        assert_eq!(
            db.query(&cx, text, &GqlParameters::new(), symbols, policy())
                .unwrap(),
            expected
        );
        assert_clean(&contexts);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn write_policy() -> fgdb_gql::GraphWriteProgramPolicy {
    fgdb_gql::GraphWriteProgramPolicy::new(policy(), 100_000, 1_000, 1_000)
}
fn allocate(request: fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    use fgdb_gql::insertion::GraphInsertRequest;
    let base = 1_000 + request.statement as u128 * 100;
    Ok(match request.request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(base + row as u128 * 10 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(base + row as u128 * 10 + edge as u128))
        }
    })
}
type ScriptError = fgdb_gql::GraphWriteScriptExecutionError<
    fgdb::WriteTxnError,
    (),
    Box<asupersync::error::Error>,
>;
fn interrupted(error: &ScriptError) -> Option<usize> {
    use fgdb_gql::{
        GqlQueryError as Qe, GraphMutationProgramError as M, GraphWriteProgramError as W,
        GraphWriteScriptExecutionError as S,
    };
    match error {
        S::Program(W::Program(M::Interrupted {
            completed_statements,
            ..
        })) => Some(*completed_statements),
        S::Program(W::Program(M::Statement {
            statement,
            source: Qe::Interrupted(_),
        }))
        | S::Program(W::Insert {
            statement,
            source: Qe::Interrupted(_),
        })
        | S::Program(W::VertexMerge {
            statement,
            source: Qe::Interrupted(_),
        })
        | S::Program(W::VertexUpsert {
            statement,
            source: Qe::Interrupted(_),
        })
        | S::Program(W::EdgeMerge {
            statement,
            source: Qe::Interrupted(_),
        })
        | S::Program(W::EdgeUpsert {
            statement,
            source: Qe::Interrupted(_),
        })
        | S::Program(W::Delete {
            statement,
            source: Qe::Interrupted(_),
        }) => Some(*statement),
        _ => None,
    }
}
fn scripts(seed: u64) -> Vec<(&'static str, String)> {
    let value = seed % 1000 + 20;
    let n = 6 + seed % 3;
    let lead = format!("CREATE (z:Person {{p:100,q:{value}}}); ");
    [
        ("insert", format!("INSERT (a:Person {{p:101,q:{value}}})")),
        ("create", format!("CREATE (a:Person {{p:102,q:{value}}})")),
        ("merge-create", format!("MATCH (a:Person),(b:Person) WHERE a.p=2 AND b.p=100 MERGE (a)-[e:R]->(b) ON CREATE SET e.q={value} ON MATCH SET e.q=999")),
        ("merge-match", format!("MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=2 MERGE (a)-[e:R]->(b) ON CREATE SET e.q=999 ON MATCH SET e.q={value}")),
        ("set", format!("MATCH (n:Person) WHERE n.p<5 SET n.q={value}")),
        ("remove", "MATCH (n:Person) WHERE n.p<5 REMOVE n.q".to_owned()),
        ("delete", format!("MATCH (n:Person) WHERE n.p={n} DELETE n")),
        ("detach", "MATCH (n:Person) WHERE n.p=2 DETACH DELETE n".to_owned()),
    ].into_iter().map(|(name, text)|(name, format!("{lead}{text}"))).collect()
}
async fn write_sweep(seed: u64, contexts: &PurposeContexts) {
    let commit = contexts.commit();
    let txcx = contexts.txn();
    let cx = contexts.query();
    for (family, source) in scripts(seed) {
        let script = fgdb_gql::PreparedGraphWriteScript::prepare(&source, R, symbols).unwrap();
        let args = GqlParameters::new();
        for open in [false, true] {
            let mut control = seeded(&commit, seed).await;
            let count = Arc::new(SimulationCheckpointProbe::new(None));
            let counted = cx.with_checkpoint_probe(count.clone());
            let expected_receipt = if open {
                let mut txn = control.begin(&txcx).unwrap();
                txn.write(&mut control, prefix()).unwrap();
                let receipt = txn
                    .execute_graph_write_script_governed(
                        &mut control,
                        &counted,
                        &script,
                        &args,
                        write_policy(),
                        allocate,
                    )
                    .unwrap();
                txn.commit(&mut control, &commit).await.unwrap();
                receipt
            } else {
                control
                    .execute_graph_write_script_autocommit_governed(
                        &txcx,
                        &counted,
                        &commit,
                        &script,
                        &args,
                        write_policy(),
                        allocate,
                    )
                    .await
                    .unwrap()
                    .0
            };
            let expected = observed(&control);
            let n = count.calls();
            assert!(n > 10, "seed={seed} family={family} open={open} N={n}");
            let ks = stops(n, seed);
            assert!(ks.len() >= 16, "family={family} open={open} N={n}");
            let mut late = false;
            for k in ks {
                let mut db = seeded(&commit, seed).await;
                let before = observed(&db);
                let seq = db.frontier().unwrap();
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(k)));
                let injected = cx.with_checkpoint_probe(probe.clone());
                let diagnostic =
                    format!("seed={seed:#x} family={family} open={open} k={k}/{n} source={source}");
                if open {
                    let mut txn = db.begin(&txcx).unwrap();
                    txn.write(&mut db, prefix()).unwrap();
                    let digest = txn.staged_effect_digest().unwrap();
                    let obligations = txcx.outstanding_obligations();
                    let error = txn
                        .execute_graph_write_script_governed(
                            &mut db,
                            &injected,
                            &script,
                            &args,
                            write_policy(),
                            allocate,
                        )
                        .expect_err(&diagnostic);
                    let completed = interrupted(&error)
                        .unwrap_or_else(|| panic!("not Interrupted: {diagnostic}: {error:?}"));
                    late |= completed > 0;
                    assert_eq!(probe.calls(), k, "{diagnostic}");
                    assert_eq!(txn.staged_effect_digest().unwrap(), digest, "{diagnostic}");
                    assert_eq!(txcx.outstanding_obligations(), obligations, "{diagnostic}");
                    check_state(&db, &before, seq);
                    txn.commit(&mut db, &commit).await.unwrap();
                    let mut prefix_control = seeded(&commit, seed).await;
                    prefix_control.write(&commit, prefix()).await.unwrap();
                    assert_eq!(
                        observed(&db),
                        observed(&prefix_control),
                        "prefix lost: {diagnostic}"
                    );
                    assert_clean(contexts);
                    check_identity(&mut db, &cx, &before);
                    let mut retry = db.begin(&txcx).unwrap();
                    let receipt = retry
                        .execute_graph_write_script_governed(
                            &mut db,
                            &cx,
                            &script,
                            &args,
                            write_policy(),
                            allocate,
                        )
                        .unwrap();
                    assert_eq!(receipt, expected_receipt, "{diagnostic}");
                    retry.commit(&mut db, &commit).await.unwrap();
                } else {
                    let error = db
                        .execute_graph_write_script_autocommit_governed(
                            &txcx,
                            &injected,
                            &commit,
                            &script,
                            &args,
                            write_policy(),
                            allocate,
                        )
                        .await
                        .expect_err(&diagnostic);
                    let completed = interrupted(&error)
                        .unwrap_or_else(|| panic!("not Interrupted: {diagnostic}: {error:?}"));
                    late |= completed > 0;
                    assert_eq!(probe.calls(), k, "{diagnostic}");
                    check_state(&db, &before, seq);
                    assert_clean(contexts);
                    check_identity(&mut db, &cx, &before);
                    let (receipt, _) = db
                        .execute_graph_write_script_autocommit_governed(
                            &txcx,
                            &cx,
                            &commit,
                            &script,
                            &args,
                            write_policy(),
                            allocate,
                        )
                        .await
                        .unwrap();
                    assert_eq!(receipt, expected_receipt, "{diagnostic}");
                }
                assert_eq!(
                    observed(&db),
                    expected,
                    "retry/control divergence: {diagnostic}"
                );
                assert_clean(contexts);
            }
            assert!(
                late,
                "no interruption after staged statement: seed={seed} family={family} open={open}"
            );
            println!(
                "seed={seed:#x} write={family} open={open} N={n} sampled={} late={late}",
                stops(n, seed).len()
            );
        }
    }
}
fn run_writes(seed: u64) {
    let ((), report) = run_async_under_lab(seed, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        write_sweep(seed, &contexts).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
#[test]
fn cancellation_writes_c101() {
    run_writes(0xc101);
}
#[test]
fn cancellation_writes_c102() {
    run_writes(0xc102);
}
#[test]
fn cancellation_writes_c103() {
    run_writes(0xc103);
}
#[test]
fn cancellation_writes_c104() {
    run_writes(0xc104);
}

#[path = "cancellation_reads/mod.rs"]
mod reads;
fn run_reads(seed: u64) {
    let ((), report) = run_async_under_lab(seed, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        reads::run(seed, &contexts).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
#[test]
fn cancellation_reads_c101() {
    run_reads(0xc101);
}
#[test]
fn cancellation_reads_c102() {
    run_reads(0xc102);
}
#[test]
fn cancellation_reads_c103() {
    run_reads(0xc103);
}
#[test]
fn cancellation_reads_c104() {
    run_reads(0xc104);
}
