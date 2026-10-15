//! Exercise the actual compiled pipeline and the original session lifecycle.
use super::*;
use crate::{Database, DatabaseKeys, MemVfs, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_beacon::{DistanceMetric, HnswConfig};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x81; 32]);
const L: LabelId = LabelId(1);
const BODY: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const NAME: PropertyKeyId = PropertyKeyId(3);
const SECRET: PropertyKeyId = PropertyKeyId(99);
const HIGH: VId = VId(u128::MAX);
const NOW: u64 = 100;
const EXPIRES: u64 = 100_000;
const CALL: &str = "CALL hybrid.search(text => 'red', text_property => 'body', \
    vector => [$x], vector_properties => ['x'], label => 'Node', eligible => $eligible, \
    candidates => 1, k => 1) YIELD node, vector_rank, text_rank ";

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn catalog(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Other") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(BODY)),
        (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn options() -> Options {
    let mut options = Options::text(BODY);
    options.vertex_label = Some(L);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
fn host() -> GqlQueryPolicy { GqlQueryPolicy::new(100, 16, 1_000_000, 100_000) }
fn grant() -> Grant {
    let mut grant = Grant::read_only("main", EXPIRES, QueryLimits {
        max_nodes: 100, max_work: 10_000_000, max_rows: 16,
    });
    grant.labels = Scope::only([L]);
    grant.properties = Scope::only([BODY, X, NAME]);
    grant
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0x81), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn parameters(x: i64, eligible: &[VId]) -> GqlParameters {
    GqlParameters::new().with_int64("x", x).unwrap().with_list(
        "eligible", eligible.iter().copied().map(GraphValue::Vertex).collect(),
    ).unwrap()
}
fn statement() -> String {
    format!("{CALL}WITH node, vector_rank, text_rank \
        RETURN node, node.name AS name, vector_rank, text_rank ORDER BY name")
}
fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Write { .. } => panic!("prepared retrieval wrote data"),
    }
}
fn expected(id: VId, name: &str) -> Vec<Vec<QueryValue>> {
    vec![vec![
        QueryValue::Value(GraphValue::Vertex(id)),
        QueryValue::Value(GraphValue::Scalar(text(name))),
        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1))),
        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1))),
    ]]
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x82; 32], NS, [0x83; 32]))
        .await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, x, name) in [(VId(1), 0, "one"), (VId(2), 1, "two"), (HIGH, 2, "high")] {
        batch.create_vertex(id, vec![L, LabelId(99)], vec![
            (BODY, text(&"red ".repeat(1024))), (X, CanonicalScalar::Int(x)),
            (NAME, text(name)), (SECRET, text("private")),
        ]);
    }
    batch.create_vertex(VId(90), vec![LabelId(99)], vec![
        (BODY, CanonicalScalar::Bool(true)), (X, text("not a coordinate")), (NAME, text("hidden")),
    ]);
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn compiled_reuse_rebinds_filters_without_resolving_symbols_or_rebuilding() {
    let ((), report) = run_async_under_lab(0xbeac_6201, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let resolutions = Cell::new(0_usize);
        let resolver = |kind, name: &str| {
            resolutions.set(resolutions.get() + 1);
            catalog(kind, name)
        };
        let mut session = db.authorized_read_session(
            &c.query(), &authority, &token, "main", resolver, host(), || NOW,
        ).unwrap();
        let index = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        let params = parameters(0, &[HIGH]);
        let prepared = session.prepare(&c.query(), &statement(), &params).unwrap();
        let bound_resolutions = resolutions.get();
        assert!(bound_resolutions > 0);
        for (id, name, x) in [(HIGH, "high", 0), (VId(2), "two", 2), (VId(1), "one", 1)] {
            let params = parameters(x, &[id]);
            assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &prepared, &params).unwrap()),
                expected(id, name));
        }
        assert!(rows(session.execute_beacon_index(
            &c.query(), &index, &prepared, &parameters(0, &[]),
        ).unwrap()).is_empty());
        assert_eq!(resolutions.get(), bound_resolutions);
        let reused = Arc::new(SimulationCheckpointProbe::new(None));
        let rebuilt = Arc::new(SimulationCheckpointProbe::new(None));
        let cached_rows = rows(session.execute_beacon_index(
            &c.query().with_checkpoint_probe(reused.clone()), &index, &prepared, &params,
        ).unwrap());
        let rebuilt_rows = rows(session.execute(
            &c.query().with_checkpoint_probe(rebuilt.clone()), &prepared, &params,
        ).unwrap());
        assert_eq!(cached_rows, expected(HIGH, "high"));
        assert_eq!(cached_rows, rebuilt_rows);
        assert!(reused.calls() < rebuilt.calls(), "reuse must avoid corpus projection/build work");
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cached_hits_and_return_properties_share_the_old_cut_after_writer_drop() {
    let ((), report) = run_async_under_lab(0xbeac_6202, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit()).await;
        let old_at = db.frontier().unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(HIGH, NAME, Some(text("new name")));
        change.set_vertex_property(HIGH, BODY, Some(text("blue")));
        db.write(&c.commit(), change).await.unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &authority, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let mut historical = options();
        historical.as_of = Some(old_at);
        let index = session.prepare_beacon_index(&c.query(), &historical).unwrap();
        let params = parameters(0, &[HIGH]);
        let prepared = session.prepare(&c.query(), &statement(), &params).unwrap();
        assert_eq!(index.source_sequence(), old_at);
        assert_ne!(rows(session.execute(&c.query(), &prepared, &params).unwrap()), expected(HIGH, "high"));
        db.compact(&c.commit()).await.unwrap();
        drop(db);
        let clone = index.clone();
        drop(index);
        assert_eq!(rows(session.execute_beacon_index(&c.query(), &clone, &prepared, &params).unwrap()),
            expected(HIGH, "high"));
        session.close();
        assert!(matches!(session.execute_beacon_index(&c.query(), &clone, &prepared, &params),
            Err(QueryError::Authorization(WardenError::ExecutionStopped))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn definitions_sources_and_raw_element_parameters_cannot_bypass_the_frozen_corpus() {
    let ((), report) = run_async_under_lab(0xbeac_6203, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &authority, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let index = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        let params = parameters(0, &[HIGH]);
        for call in [
            CALL.replace("label => 'Node'", "label => 'Other'"),
            CALL.replace("text_property => 'body'", "text_property => 'name'"),
            CALL.replace("vector_properties => ['x']", "vector_properties => ['name']"),
            CALL.replace("vector_properties => ['x']", "vector_properties => ['x'], vector_encoding => 'coordinates-rounded'"),
            CALL.replace("vector_properties => ['x']", "vector_properties => ['x'], metric => 'dot'"),
            CALL.replace("vector => [$x],", "seeds => [1], max_hops => 0, vector => [$x],"),
        ] {
            let prepared = session.prepare(&c.query(), &format!("{call}RETURN node LIMIT 0"), &params).unwrap();
            assert!(session.execute_beacon_index(&c.query(), &index, &prepared, &params).is_err(), "{call}");
            assert!(!session.is_closed());
        }
        // Match has its own public entrypoint; this profile cannot silently
        // replace an unsupported source with a scan over the session snapshot.
        let pattern = session.prepare(&c.query(), "MATCH (n:Node) RETURN n.name", &GqlParameters::new()).unwrap();
        assert!(session.execute_beacon_index(&c.query(), &index, &pattern, &GqlParameters::new()).is_err());
        let masked = session.prepare(&c.query(), &format!("{CALL}RETURN node.secret AS secret"), &params).unwrap();
        assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &masked, &params).unwrap()),
            vec![vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null))]]);
        let attack_params = parameters(0, &[HIGH]).with_list(
            "supplied", vec![GraphValue::Vertex(VId(90))],
        ).unwrap();
        let attack = session.prepare(&c.query(), &format!(
            "{CALL}WITH $supplied[0] AS outside RETURN outside.name AS name"
        ), &attack_params).unwrap();
        assert!(session.execute_beacon_index(&c.query(), &index, &attack, &attack_params).is_err());
        let valid = session.prepare(&c.query(), &statement(), &params).unwrap();
        assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &valid, &params).unwrap()),
            expected(HIGH, "high"));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn both_handle_owners_and_live_expiry_are_checked_even_on_empty_answers() {
    let ((), report) = run_async_under_lab(0xbeac_6204, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let now = Cell::new(NOW);
        let mut first = db.authorized_read_session(
            &c.query(), &authority, &token, "main", catalog, host(), || now.get(),
        ).unwrap();
        let mut second = db.authorized_read_session(
            &c.query(), &authority, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let index = first.prepare_beacon_index(&c.query(), &options()).unwrap();
        let other = second.prepare_beacon_index(&c.query(), &options()).unwrap();
        let params = parameters(0, &[]);
        let prepared = first.prepare(&c.query(), &statement(), &params).unwrap();
        let own_plan = second.prepare(&c.query(), &statement(), &params).unwrap();
        assert!(matches!(second.execute_beacon_index(&c.query(), &other, &prepared, &params),
            Err(QueryError::Authorization(WardenError::WrongAuthority))));
        assert!(matches!(second.execute_beacon_index(&c.query(), &index, &own_plan, &params),
            Err(QueryError::Authorization(WardenError::ScopeDenied))));
        assert!(!second.is_closed());
        now.set(EXPIRES + 1);
        assert!(matches!(first.execute_beacon_index(&c.query(), &index, &prepared, &params),
            Err(QueryError::Authorization(WardenError::Expired))));
        assert!(first.is_closed());
        authority.retire();
        assert!(matches!(second.execute_beacon_index(&c.query(), &index, &prepared, &params),
            Err(QueryError::Authorization(WardenError::AuthorityRetired))));
        assert!(second.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn set_operands_share_node_and_record_allowances_instead_of_fresh_permits() {
    let ((), report) = run_async_under_lab(0xbeac_6205, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let authority = authority();
        let params = parameters(0, &[HIGH]);
        let branch = format!("{CALL}RETURN node");
        let union = format!("{branch} UNION ALL {branch}");
        for signed in [false, true] {
            let mut allowance = grant();
            if signed { allowance.limits.max_nodes = 5; }
            let token = authority.issue_at(&allowance, NOW).unwrap();
            let mut policy = host();
            if !signed { policy = GqlQueryPolicy::new(5, 16, 1_000_000, 100_000); }
            let mut session = db.authorized_read_session(
                &c.query(), &authority, &token, "main", catalog, policy, || NOW,
            ).unwrap();
            let index = session.prepare_beacon_index(&c.query(), &options()).unwrap();
            let single = session.prepare(&c.query(), &branch, &params).unwrap();
            let both = session.prepare(&c.query(), &union, &params).unwrap();
            assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &single, &params).unwrap()).len(), 1);
            assert!(session.execute_beacon_index(&c.query(), &index, &both, &params).is_err());
            assert!(!session.is_closed());
            assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &single, &params).unwrap()).len(), 1);
        }
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &authority, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let index = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        let both = session.prepare(&c.query(), &union, &params).unwrap();
        assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &both, &params).unwrap()).len(), 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancellation_late_expression_and_invalid_eligibility_never_release_a_prefix() {
    let ((), report) = run_async_under_lab(0xbeac_6206, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &authority, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let index = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        let params = parameters(0, &[HIGH]);
        let prepared = session.prepare(&c.query(), &statement(), &params).unwrap();
        for eligible in [vec![HIGH], vec![]] {
            let params = parameters(0, &eligible);
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let expected = rows(session.execute_beacon_index(
                &c.query().with_checkpoint_probe(probe.clone()), &index, &prepared, &params,
            ).unwrap());
            assert!(probe.calls() > 5);
            for cut in 1..=probe.calls() {
                let stopped = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
                assert!(session.execute_beacon_index(
                    &c.query().with_checkpoint_probe(stopped.clone()), &index, &prepared, &params,
                ).is_err(), "cut {cut}");
                assert!(stopped.calls() >= cut);
                assert!(!session.is_closed());
                assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &prepared, &params).unwrap()), expected);
            }
        }
        for tail in ["RETURN 1 / 0 AS bad LIMIT 0", "RETURN node LIMIT 0"] {
            let prepared = session.prepare(&c.query(), &format!("{CALL}{tail}"), &params).unwrap();
            if tail.contains("1 / 0") {
                assert!(session.execute_beacon_index(&c.query(), &index, &prepared, &params).is_err());
            } else {
                assert!(rows(session.execute_beacon_index(&c.query(), &index, &prepared, &params).unwrap()).is_empty());
                let invalid = GqlParameters::new().with_int64("x", 0).unwrap().with_list(
                    "eligible", vec![GraphValue::Scalar(CanonicalScalar::Int(-1))],
                ).unwrap();
                assert!(session.execute_beacon_index(&c.query(), &index, &prepared, &invalid).is_err());
            }
        }
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
