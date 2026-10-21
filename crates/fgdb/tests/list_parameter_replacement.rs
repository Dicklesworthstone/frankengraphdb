//! Declared list parameters keep their domain inside property replacement maps.
//! Exercise the ordinary mutation and transaction owners, not a second evaluator.

use asupersync::Budget;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlListParameter, GqlMapParameter, GqlParameterType, GqlParameterValue, GqlParameters,
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GqlScalarParameter, GraphMutationPolicy,
    GraphSymbol, GraphSymbolKind, GraphSymbolResolver, PreparedGraphMutationQueryText,
    PreparedGraphMutationText, ReverseSymbolCatalog,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, TxnCx,
    VId,
};

const R: RelationId = RelationId(1);
const ITEM: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const BASE: u128 = 1_u128 << 100;

struct Catalog;
impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            _ => None,
        }
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_relation(R, "R");
        catalog.insert_label(ITEM, "Item");
        catalog.insert_property(P, "p");
        catalog.insert_property(Q, "q");
        Some(catalog)
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}

fn arguments(value: GqlParameterValue) -> GqlParameters {
    let mut arguments = GqlParameters::new();
    arguments.insert("items", value).unwrap();
    arguments
}

fn listed(values: Vec<GraphValue>) -> GqlParameters {
    arguments(GqlParameterValue::List(
        GqlListParameter::new(values).unwrap(),
    ))
}

fn policy(rows: u64) -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(1_000, rows, 1_000_000, 1_000_000), 100)
}

fn fixture() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for n in 1..=2_i64 {
        batch.create_vertex(
            VId(BASE + n as u128),
            vec![ITEM],
            vec![
                (P, CanonicalScalar::Int(n)),
                (Q, CanonicalScalar::Int(n * 10)),
            ],
        );
    }
    batch.add_edge(
        EId(BASE + 10),
        VId(BASE + 1),
        VId(BASE + 2),
        vec![(P, CanonicalScalar::Int(7)), (Q, CanonicalScalar::Int(70))],
    );
    batch
}

fn rows(execution: &GqlQueryExecution<GraphValueRow>) -> Vec<Vec<GraphValue>> {
    execution
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

fn run<T>(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx) -> T) -> T {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let query = contexts.query();
    let txn = contexts.txn();
    runtime.block_on(test(&commit, &query, &txn))
}

#[test]
fn replacement_parameters_rebind_nested_empty_and_null_values_and_reopen() {
    let template = PreparedGraphMutationQueryText::prepare_with_parameter_types(
        "MATCH (n:Item) SET n = {q: $items} \
         RETURN n AS node,n.q AS q,properties(n) AS document ORDER BY node",
        R,
        &[("items", GqlParameterType::List)],
        Catalog,
    )
    .unwrap();
    assert_eq!(template.parameter_schema().len(), 1);
    assert_eq!(
        template.parameter_schema()[0].parameter_type,
        GqlParameterType::List
    );
    let nested = vec![
        int(9),
        list(vec![int(4), GraphValue::Scalar(CanonicalScalar::Null)]),
    ];
    let cases = [
        (listed(nested.clone()), list(nested)),
        (listed(Vec::new()), list(Vec::new())),
        (
            arguments(GqlParameterValue::Scalar(
                GqlScalarParameter::new(CanonicalScalar::Null).unwrap(),
            )),
            GraphValue::Scalar(CanonicalScalar::Null),
        ),
    ];
    let mut definitions = std::collections::BTreeSet::new();
    for (arguments, expected) in cases {
        let query = template.bind_parameters(&arguments).unwrap();
        assert!(definitions.insert(query.canonical_bytes()));
        run(async |commit, cx, txcx| {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            db.write(commit, fixture()).await.unwrap();
            let mut tx = db.begin(txcx).unwrap();
            let (stats, output) = tx
                .execute_graph_mutation_query_governed(&mut db, cx, &query, policy(2))
                .unwrap();
            assert_eq!(stats.target_vertices, 2);
            assert_eq!(stats.effects, 4, "each target sets q and removes p");
            let document = GraphValue::map(vec![("q".into(), expected.clone())]).unwrap();
            assert_eq!(
                rows(&output),
                vec![
                    vec![
                        GraphValue::Vertex(VId(BASE + 1)),
                        expected.clone(),
                        document.clone()
                    ],
                    vec![
                        GraphValue::Vertex(VId(BASE + 2)),
                        expected.clone(),
                        document
                    ],
                ]
            );
            tx.commit(&mut db, commit).await.unwrap();
            db.compact(commit).await.unwrap();
            drop(db);
            let db = Database::open_with_vfs(commit, vfs, &path, keys())
                .await
                .unwrap();
            for id in [BASE + 1, BASE + 2] {
                assert_eq!(
                    db.vertex(VId(id)).unwrap().unwrap().props,
                    vec![(Q, expected.to_written_property().unwrap())]
                );
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
    }
}

#[test]
fn no_return_set_and_merge_maps_preserve_omitted_fields_only_when_requested() {
    for (tail, replace) in [
        ("SET n.q=$items", false),
        ("SET n += {q:$items}", false),
        ("SET n = {q:$items}", true),
    ] {
        let template = PreparedGraphMutationText::prepare_with_parameter_types(
            &format!("MATCH (n:Item) {tail}"),
            R,
            &[("items", GqlParameterType::List)],
            Catalog,
        )
        .unwrap();
        let mutation = template.bind_parameters(&listed(vec![int(5)])).unwrap();
        run(async |commit, cx, txcx| {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            db.write(commit, fixture()).await.unwrap();
            let mut tx = db.begin(txcx).unwrap();
            let stats = tx
                .execute_graph_mutation_governed(&mut db, cx, &mutation, policy(2))
                .unwrap();
            assert_eq!(stats.effects, if replace { 4 } else { 2 });
            tx.commit(&mut db, commit).await.unwrap();
            for n in 1..=2_i64 {
                let mut expected = Vec::new();
                if !replace {
                    expected.push((P, CanonicalScalar::Int(n).into()));
                }
                expected.push((Q, list(vec![int(5)]).to_written_property().unwrap()));
                assert_eq!(
                    db.vertex(VId(BASE + n as u128)).unwrap().unwrap().props,
                    expected
                );
            }
        });
    }
}

#[test]
fn relationship_replacement_returns_final_lists_without_modifying_its_endpoints() {
    let query = PreparedGraphMutationQueryText::prepare_with_parameter_types(
        "MATCH (a)-[r:R]->(b) SET r = {q:$items} \
         RETURN r.q AS q,properties(r) AS document",
        R,
        &[("items", GqlParameterType::List)],
        Catalog,
    )
    .unwrap()
    .bind_parameters(&listed(vec![int(3), int(8)]))
    .unwrap();
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let vertices = db.vertices().unwrap();
        let mut tx = db.begin(txcx).unwrap();
        let (stats, output) = tx
            .execute_graph_mutation_query_governed(&mut db, cx, &query, policy(1))
            .unwrap();
        assert_eq!(
            (stats.target_vertices, stats.target_edges, stats.effects),
            (0, 1, 2)
        );
        let expected = list(vec![int(3), int(8)]);
        assert_eq!(
            rows(&output),
            vec![vec![
                expected.clone(),
                GraphValue::map(vec![("q".into(), expected.clone())]).unwrap(),
            ]]
        );
        tx.commit(&mut db, commit).await.unwrap();
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(
            db.edge(EId(BASE + 10)).unwrap().unwrap().props,
            vec![(Q, expected.to_written_property().unwrap())]
        );
    });
}

#[test]
fn declared_list_binding_rejects_wrong_missing_extra_and_map_arguments() {
    let statement = "MATCH (n:Item) SET n = {q:$items} RETURN n.q AS q";
    let template = PreparedGraphMutationQueryText::prepare_with_parameter_types(
        statement,
        R,
        &[("items", GqlParameterType::List)],
        Catalog,
    )
    .unwrap();
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    assert!(
        template
            .bind_parameters(&arguments(GqlParameterValue::Int64(4)))
            .is_err()
    );
    let map = GqlMapParameter::new(vec![("nested".into(), int(4))]).unwrap();
    assert!(
        template
            .bind_parameters(&arguments(GqlParameterValue::Map(map.clone())))
            .is_err()
    );
    let mut extra = listed(vec![int(4)]);
    extra.insert("unused", GqlParameterValue::Int64(1)).unwrap();
    assert!(template.bind_parameters(&extra).is_err());
    assert!(template.bind_parameters(&listed(vec![int(4)])).is_ok());
    // A list-only fix must not silently admit a map-valued property.
    let map_template = PreparedGraphMutationQueryText::prepare_with_parameter_types(
        statement,
        R,
        &[("items", GqlParameterType::Map)],
        Catalog,
    )
    .unwrap();
    assert!(
        map_template
            .bind_parameters(&arguments(GqlParameterValue::Map(map)))
            .is_err()
    );
    // The established scalar parameter route is still usable and exactly typed.
    let scalar = PreparedGraphMutationQueryText::prepare_with_parameter_types(
        statement,
        R,
        &[("items", GqlParameterType::Int64)],
        Catalog,
    )
    .unwrap();
    assert!(
        scalar
            .bind_parameters(&arguments(GqlParameterValue::Int64(4)))
            .is_ok()
    );
    assert!(scalar.bind_parameters(&listed(vec![int(4)])).is_err());
}

#[test]
fn invalid_list_members_and_output_limits_do_not_publish_replacement_effects() {
    let template = PreparedGraphMutationQueryText::prepare_with_parameter_types(
        "MATCH (n:Item) SET n = {q:$items} RETURN n.q AS q LIMIT 0",
        R,
        &[("items", GqlParameterType::List)],
        Catalog,
    )
    .unwrap();
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.vertices().unwrap();
        let frontier = db.frontier().unwrap();
        let mut tx = db.begin(txcx).unwrap();
        for invalid in [
            GraphValue::map(vec![("bad".into(), int(1))]).unwrap(),
            GraphValue::Vertex(VId(BASE + 1)),
        ] {
            let query = template.bind_parameters(&listed(vec![invalid])).unwrap();
            assert!(matches!(
                tx.execute_graph_mutation_query_governed(&mut db, cx, &query, policy(0)),
                Err(GqlQueryError::Source(_))
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(db.vertices().unwrap(), before);
        }
        let unpaged = PreparedGraphMutationQueryText::prepare_with_parameter_types(
            "MATCH (n:Item) SET n = {q:$items} RETURN n.q AS q",
            R,
            &[("items", GqlParameterType::List)],
            Catalog,
        )
        .unwrap()
        .bind_parameters(&listed(vec![int(4)]))
        .unwrap();
        assert!(matches!(
            tx.execute_graph_mutation_query_governed(&mut db, cx, &unpaged, policy(1)),
            Err(GqlQueryError::Rows(_))
        ));
        let valid = template.bind_parameters(&listed(vec![int(4)])).unwrap();
        let (stats, output) = tx
            .execute_graph_mutation_query_governed(&mut db, cx, &valid, policy(0))
            .unwrap();
        assert_eq!(stats.effects, 4);
        assert!(output.value.is_empty());
        tx.commit(&mut db, commit).await.unwrap();
        for id in [BASE + 1, BASE + 2] {
            assert_eq!(
                db.vertex(VId(id)).unwrap().unwrap().props,
                vec![(Q, list(vec![int(4)]).to_written_property().unwrap())]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn late_return_error_rolls_back_only_its_statement_then_the_same_template_succeeds() {
    let template = PreparedGraphMutationQueryText::prepare_with_parameter_types(
        "MATCH (n:Item) SET n = {p:n.p,q:$items} \
         RETURN n.q AS q,12/(n.p-$divisor) AS bad LIMIT 0",
        R,
        &[
            ("items", GqlParameterType::List),
            ("divisor", GqlParameterType::Int64),
        ],
        Catalog,
    )
    .unwrap();
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.vertices().unwrap();
        let frontier = db.frontier().unwrap();
        let mut tx = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(BASE + 99), Vec::new(), Vec::new());
        tx.write(&mut db, prefix).unwrap();
        let digest = tx.staged_effect_digest().unwrap();
        let mut args = listed(vec![int(6)]);
        args.insert("divisor", GqlParameterValue::Int64(2)).unwrap();
        let failing = template.bind_parameters(&args).unwrap();
        assert!(matches!(
            tx.execute_graph_mutation_query_governed(&mut db, cx, &failing, policy(0)),
            Err(GqlQueryError::Source(_))
        ));
        assert_eq!(tx.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap(), before);
        let mut args = listed(vec![int(6)]);
        args.insert("divisor", GqlParameterValue::Int64(100))
            .unwrap();
        let passing = template.bind_parameters(&args).unwrap();
        let (_, output) = tx
            .execute_graph_mutation_query_governed(&mut db, cx, &passing, policy(0))
            .unwrap();
        assert!(output.value.is_empty());
        tx.commit(&mut db, commit).await.unwrap();
        assert!(db.vertex(VId(BASE + 99)).unwrap().is_some());
        for n in 1..=2_i64 {
            assert_eq!(
                db.vertex(VId(BASE + n as u128)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(n).into()),
                    (Q, list(vec![int(6)]).to_written_property().unwrap()),
                ]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}
