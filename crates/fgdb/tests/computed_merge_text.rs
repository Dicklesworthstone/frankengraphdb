//! Native MERGE expressions, atomic UNWIND and capability-scoped scalar inputs.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::unwind_write::{GraphUnwindWriteExecutionError, GraphUnwindWriteText};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphVertexMergePolicy, GraphVertexUpsertError, GraphVertexUpsertPolicy,
    GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy, PreparedGraphVertexUpsertText,
    PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar as Scalar, CanonicalScalarKind, DatabaseSecurityNamespaceId,
    EmbeddedTxnCompletion, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const ID: PropertyKeyId = PropertyKeyId(1);
const COUNT: PropertyKeyId = PropertyKeyId(2);
const NAME: PropertyKeyId = PropertyKeyId(3);
const READY: PropertyKeyId = PropertyKeyId(4);
const COPY: PropertyKeyId = PropertyKeyId(5);
const SECRET: PropertyKeyId = PropertyKeyId(6);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xc2; 32]);
const BATCH: &str = "UNWIND $rows AS row MERGE (n:Person {id:row.id}) \
    ON CREATE SET n.count=0 SET n.count=n.count+row.delta";

fn keys() -> DatabaseKeys { DatabaseKeys::new([0xc1; 32], NS, [0xc3; 32]) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, name) => match name {
            "id" => Some(ID), "count" => Some(COUNT), "name" => Some(NAME),
            "ready" => Some(READY), "copy" => Some(COPY), "secret" => Some(SECRET),
            _ => None,
        }.map(GraphSymbol::Property),
        _ => None,
    }
}
fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(20_000, 20_000, 2_000_000, 2_000_000)
}
fn program_policy(effects: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(), effects, 10, 0)
}
fn scalar(db: &Database<MemVfs>, vertex: VId, key: PropertyKeyId) -> Scalar {
    db.vertex(vertex).unwrap().unwrap().props.into_iter()
        .find(|(actual, _)| *actual == key).unwrap().1
}
fn rows(values: &[(i64, i64)]) -> GqlParameters {
    let values = values.iter().map(|&(id, delta)| GraphValue::map(vec![
        ("id".into(), GraphValue::Scalar(Scalar::Int(id))),
        ("delta".into(), GraphValue::Scalar(Scalar::Int(delta))),
    ]).unwrap()).collect();
    GqlParameters::new().with_list("rows", values).unwrap()
}

#[test]
fn prepared_text_reuses_parameters_and_compiles_scalar_case_and_property_reads() {
    let ((), report) = run_async_under_lab(0x5c73_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let text = "MERGE (n:Person {id:$id}) ON CREATE SET n.count=0 \
            SET n.count=coalesce(n.count,0)+$delta, n.name=UPPER($name), \
            n.ready=CASE WHEN $delta>0 THEN TRUE ELSE FALSE END, n.copy=n.count";
        let template = PreparedGraphVertexUpsertText::prepare_with_parameter_types(
            text, R, &[("id", GqlParameterType::Int64), ("delta", GqlParameterType::Int64),
                ("name", GqlParameterType::Scalar(CanonicalScalarKind::Text))], symbols,
        ).unwrap();
        assert_eq!(template.parameter_schema().len(), 3);
        for (delta, name, expected, copied) in [(3, "Ada", 3, 0), (4, "Eve", 7, 3)] {
            let arguments = GqlParameters::new().with_int64("id", 1).unwrap()
                .with_int64("delta", delta).unwrap().with_text("name", name).unwrap();
            let frozen = arguments.canonical_bytes();
            let upsert = template.bind_parameters(&arguments).unwrap();
            let (_, outcome, _) = db.execute_graph_vertex_upsert_autocommit_governed(
                &txn, &query, &commit, &upsert,
                GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(query_policy()), 8),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(1))),
            ).await.unwrap();
            assert_eq!(outcome.created(), copied == 0);
            assert_eq!(scalar(&db, VId(1), COUNT), Scalar::Int(expected));
            assert_eq!(scalar(&db, VId(1), COPY), Scalar::Int(copied));
            assert_eq!(scalar(&db, VId(1), NAME), Scalar::ucs_basic_text(&name.to_uppercase()).unwrap());
            assert_eq!(scalar(&db, VId(1), READY), Scalar::Bool(true));
            assert_eq!(arguments.canonical_bytes(), frozen);
        }
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn malformed_expression_scopes_and_declared_types_refuse_before_catalog_callbacks() {
    for text in [
        "MERGE (n:Person {id:1}) SET n.count=other.count+1",
        "MERGE (n:Person {id:1}) SET n.count=n",
        "MERGE (n:Person {id:1}) SET n.count=n.count.part",
        "MERGE (n:Person {id:1}) SET n.count=coalesce(n.count,)",
        "MERGE (n:Person {id:1}) SET n.count=[1,2]",
        "MERGE (n:Person {id:1}) SET n.count=$s*2",
    ] {
        let mut calls = 0;
        let result = PreparedGraphVertexUpsertText::prepare_with_parameter_types(
            text, R, &[("s", GqlParameterType::Scalar(CanonicalScalarKind::Text))],
            |kind, name| { calls += 1; symbols(kind, name) },
        );
        assert!(result.is_err(), "{text}");
        assert_eq!(calls, 0, "{text}");
    }
    let text = "MERGE (n:Person {id:$id}) SET n.count=n.count+$delta";
    let template = PreparedGraphVertexUpsertText::prepare(text, R, symbols).unwrap();
    assert!(template.bind_parameters(&GqlParameters::new().with_int64("id", 1).unwrap()).is_err());
    assert!(template.bind_parameters(&GqlParameters::new().with_int64("id", 1).unwrap()
        .with_text("delta", "not an integer").unwrap()).is_err());
}

#[test]
fn unwind_repeated_keys_accumulate_in_one_commit_and_survive_reopen() {
    let ((), report) = run_async_under_lab(0x5c73_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let definition = GraphUnwindWriteText::parse(BATCH).unwrap();
        let arguments = rows(&[(1, 3), (2, 5), (1, 7)]);
        let frozen = arguments.canonical_bytes();
        let mut allocations = 0;
        let (receipt, completion) = db.execute_graph_unwind_write_autocommit_governed(
            &txn, &query, &commit, &definition, &arguments, R, 3, symbols,
            program_policy(5), |_| {
                allocations += 1;
                Ok::<_, ()>(ElementId::Vertex(VId(allocations)))
            },
        ).await.unwrap();
        assert_eq!(allocations, 2);
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(receipt.stats().mutation_effects, 5);
        assert_eq!(arguments.canonical_bytes(), frozen);
        assert!(matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 1));
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(scalar(&db, VId(1), COUNT), Scalar::Int(10));
        assert_eq!(scalar(&db, VId(2), COUNT), Scalar::Int(5));
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_unwind_overflow_or_shared_action_limit_never_publishes_a_prefix() {
    let ((), report) = run_async_under_lab(0x5c73_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        for (input, effects) in [(rows(&[(1, i64::MAX), (1, 1)]), 3),
                                (rows(&[(1, 1), (1, 1)]), 2)] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let definition = GraphUnwindWriteText::parse(BATCH).unwrap();
            let mut allocations = 0;
            let result = db.execute_graph_unwind_write_autocommit_governed(
                &txn, &query, &commit, &definition, &input, R, 2, symbols,
                program_policy(effects), |_| {
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(1)))
                },
            ).await;
            assert!(matches!(result, Err(GraphUnwindWriteExecutionError::Execution(_))));
            assert_eq!(allocations, 1, "first row ran, second did not allocate");
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn overwritten_failing_on_assignment_is_not_erased_but_dead_case_arm_is_lazy() {
    let ((), report) = run_async_under_lab(0x5c73_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let bad = PreparedGraphVertexUpsertText::prepare(
            "MERGE (n:Person {id:1}) ON CREATE SET n.count=1/0 SET n.count=7", R, symbols,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let result = db.execute_graph_vertex_upsert_autocommit_governed(
            &txn, &query, &commit, &bad,
            GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(query_policy()), 8),
            |_| Ok::<_, ()>(ElementId::Vertex(VId(1))),
        ).await;
        assert!(matches!(result, Err(GqlQueryError::Source(GraphVertexUpsertError::Expression {
            clause: 0, action: 0, ..
        }))));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        let good = PreparedGraphVertexUpsertText::prepare(
            "MERGE (n:Person {id:1}) SET n.count=CASE WHEN TRUE THEN 7 ELSE 1/0 END", R, symbols,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        db.execute_graph_vertex_upsert_autocommit_governed(
            &txn, &query, &commit, &good,
            GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(query_policy()), 8),
            |_| Ok::<_, ()>(ElementId::Vertex(VId(2))),
        ).await.unwrap();
        assert_eq!(scalar(&db, VId(2), COUNT), Scalar::Int(7));
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9982), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(), labels: Scope::only([PERSON]), relations: Scope::only([R]),
        properties: Scope::only([ID, COUNT]), rights: Rights::ReadWrite,
        limits: QueryLimits { max_nodes: 10_000, max_work: 2_000_000, max_rows: 0 },
        expires_at_ms: 10_000,
    }
}

#[test]
fn authorized_computed_reads_mask_hidden_payloads_before_value_or_size_admission() {
    let ((), report) = run_async_under_lab(0x5c73_0014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let program = PreparedGraphWriteScript::prepare(
            "MERGE (n:Person {id:1}) SET n.count=coalesce(n.secret,0)+1", R, symbols,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let mut first_stats = None;
        for secret in [Scalar::Int(99), Scalar::ucs_basic_text(&"x".repeat(8192)).unwrap()] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![PERSON], vec![(ID, Scalar::Int(1)), (SECRET, secret.clone())]);
            db.write(&commit, seed).await.unwrap();
            let (stats, _) = db.execute_graph_write_program_authorized(
                &txn, &query, &commit, &authority, &token, "main", &program,
                program_policy(1), || 100,
            ).await.unwrap();
            assert_eq!(scalar(&db, VId(1), COUNT), Scalar::Int(1));
            assert_eq!(scalar(&db, VId(1), SECRET), secret);
            if let Some(expected) = &first_stats { assert_eq!(&stats, expected); }
            else { first_stats = Some(stats); }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_on_clause_write_cannot_be_hidden_by_a_trailing_overwrite() {
    let ((), report) = run_async_under_lab(0x5c73_0015, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![(ID, Scalar::Int(1)), (SECRET, Scalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let original = db.vertex(VId(1)).unwrap();
        let program = PreparedGraphWriteScript::prepare(
            "MERGE (n:Person {id:1}) ON MATCH SET n.secret=1 SET n.secret=7", R, symbols,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        assert!(db.execute_graph_write_program_authorized(
            &txn, &query, &commit, &authority, &token, "main", &program,
            program_policy(2), || 100,
        ).await.is_err());
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.vertex(VId(1)).unwrap(), original);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
