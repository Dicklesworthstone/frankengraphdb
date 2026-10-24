//! Composite SET expressions through the durable public write path (fgdb-txbg9).
//! Expected values are derived from the fixture, not from an equivalent query.

use asupersync::Budget;
use fgdb::{Database, DatabaseKeys, PinnedTzdb, QueryResult, QueryValue, TzdbZone, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GqlTimeZones, GraphSymbol, GraphSymbolKind, GraphSymbolResolver,
    GraphWriteProgramPolicy, PreparedGraphMutationText,
};
use fgdb_types::{
    CanonicalDate, CanonicalDuration, CanonicalLocalDateTime, CanonicalLocalTime, CanonicalScalar,
    CanonicalTime, CanonicalTimestamp, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, TxnCx, VId,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const D: PropertyKeyId = PropertyKeyId(3);

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
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Missing") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "d") => Some(GraphSymbol::Property(D)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 1_000_000)
}

fn write_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(policy(), 100_000, 100, 100)
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

fn fixture() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for id in [1, 2] {
        batch.create_vertex(
            VId(id as u128),
            vec![PERSON],
            vec![
                (P, CanonicalScalar::Int(id)),
                (Q, CanonicalScalar::Int(id * 10)),
            ],
        );
    }
    batch.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(7))]);
    batch
}

fn scalar(value: CanonicalScalar) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(value))
}

fn int(value: i64) -> QueryValue {
    scalar(CanonicalScalar::Int(value))
}

fn text(value: &str) -> QueryValue {
    scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}

fn rows(names: &[&str], values: Vec<Vec<QueryValue>>) -> QueryResult {
    QueryResult::Rows {
        columns: names.iter().map(|name| (*name).to_owned()).collect(),
        rows: values,
    }
}

#[test]
fn simultaneous_vertex_and_edge_set_results_survive_reopen() {
    run(async |commit, query, txn| {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fgdb-composite-set-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let mut db = Database::create(commit, &path, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let params = GqlParameters::new();
        let updated = db
            .query_write(
                txn,
                query,
                commit,
                "MATCH (n:Person) SET n.p = head([n.q]), \
                 n.q = reduce(s = 0, x IN [n.p, 2] | s + x), \
                 n.d = date({year: 2024, month: 2, day: 29}) \
                 RETURN n.p AS p, n.q AS q, toString(n.d) AS d ORDER BY p",
                &params,
                symbols,
                R,
                write_policy(),
                |_| Err::<ElementId, _>("SET must not allocate identities"),
            )
            .await
            .unwrap();
        let expected = rows(
            &["p", "q", "d"],
            vec![
                vec![int(10), int(3), text("2024-02-29")],
                vec![int(20), int(4), text("2024-02-29")],
            ],
        );
        assert_eq!(updated, expected);
        let edge = db
            .query_write(
                txn,
                query,
                commit,
                "MATCH ()-[r:R]->() SET r += {p: head([r.p]) + 3} RETURN r.p AS p",
                &params,
                symbols,
                R,
                write_policy(),
                |_| Err::<ElementId, _>("SET must not allocate identities"),
            )
            .await
            .unwrap();
        assert_eq!(edge, rows(&["p"], vec![vec![int(10)]]));
        drop(db);
        let db = Database::open(commit, &path, keys()).await.unwrap();
        assert_eq!(
            db.query(
                query,
                "MATCH (n:Person) RETURN n.p AS p, n.q AS q, toString(n.d) AS d ORDER BY p",
                &params,
                symbols,
                policy(),
            )
            .unwrap(),
            expected
        );
        assert_eq!(
            db.query(
                query,
                "MATCH ()-[r:R]->() RETURN r.p AS p",
                &params,
                symbols,
                policy()
            )
            .unwrap(),
            edge
        );
    });
}

#[test]
fn a_late_temporal_error_rolls_back_the_complete_write_script() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.frontier().unwrap();
        let snapshot = [
            db.vertex(VId(1)).unwrap().map(|row| row.props.clone()),
            db.vertex(VId(2)).unwrap().map(|row| row.props.clone()),
        ];
        let script = "MATCH (n:Person) SET n.p = head([n.p]) + 100; \
                      MATCH (n:Person) SET n.d = date({year: 2024, month: 2, day: $day})";
        let error = db
            .query_write(
                txn,
                query,
                commit,
                script,
                &GqlParameters::new().with_int64("day", 30).unwrap(),
                symbols,
                R,
                write_policy(),
                |_| Err::<ElementId, _>("SET must not allocate identities"),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("TemporalConstruction"),
            "{error}"
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(
            [
                db.vertex(VId(1)).unwrap().map(|row| row.props.clone()),
                db.vertex(VId(2)).unwrap().map(|row| row.props.clone()),
            ],
            snapshot
        );
        // Same compiled shape, valid data: it must commit, not refuse both cases.
        let result = db
            .query_write(
                txn,
                query,
                commit,
                script,
                &GqlParameters::new().with_int64("day", 29).unwrap(),
                symbols,
                R,
                write_policy(),
                |_| Err::<ElementId, _>("SET must not allocate identities"),
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            QueryResult::Write {
                completion: Some(_),
                ..
            }
        ));
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(
            db.query(
                query,
                "MATCH (n:Person) RETURN n.p AS p ORDER BY p",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap(),
            rows(&["p"], vec![vec![int(101)], vec![int(102)]])
        );
    });
}

#[test]
fn limit_zero_cannot_hide_a_non_scalar_assignment_or_publish_its_prefix() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.frontier().unwrap();
        let snapshot = [
            db.vertex(VId(1)).unwrap().map(|row| row.props.clone()),
            db.vertex(VId(2)).unwrap().map(|row| row.props.clone()),
        ];
        let error = db
            .query_write(
                txn,
                query,
                commit,
                "MATCH (n:Person) SET n.q = 999, n.d = [{value:n.p}][0] RETURN n.q AS q LIMIT 0",
                &GqlParameters::new(),
                symbols,
                R,
                write_policy(),
                |_| Err::<ElementId, _>("SET must not allocate identities"),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("not a scalar property value"),
            "{error}"
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(
            [
                db.vertex(VId(1)).unwrap().map(|row| row.props.clone()),
                db.vertex(VId(2)).unwrap().map(|row| row.props.clone()),
            ],
            snapshot
        );
    });
}

#[test]
fn absent_optional_targets_do_not_evaluate_composite_assignments() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = db
            .query_write(
                txn,
                query,
                commit,
                "OPTIONAL MATCH (n:Missing) SET n.d = date({year: 2024, month: 2, day: 30}) \
                 RETURN n.d AS d",
                &GqlParameters::new(),
                symbols,
                R,
                write_policy(),
                |_| Err::<ElementId, _>("SET must not allocate identities"),
            )
            .await
            .unwrap();
        assert_eq!(
            result,
            rows(&["d"], vec![vec![scalar(CanonicalScalar::Null)]])
        );
    });
}

#[test]
fn every_version_two_temporal_kind_is_stored_without_text_coercion() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let date = CanonicalDate::from_civil(2015, 7, 21).unwrap();
        let time = CanonicalLocalTime::from_hms_nano(21, 40, 32, 142_000_000).unwrap();
        let cases = [
            ("date('2015-07-21')", CanonicalScalar::Date(date)),
            (
                "localtime('21:40:32.142')",
                CanonicalScalar::LocalTime(time),
            ),
            (
                "time('21:40:32.142+01:00')",
                CanonicalScalar::Time(CanonicalTime::new(time, 3_600).unwrap()),
            ),
            (
                "localdatetime('2015-07-21T21:40:32.142')",
                CanonicalScalar::LocalDateTime(CanonicalLocalDateTime::new(date, time)),
            ),
            (
                "duration({months: 14, days: -3, nanoseconds: 5})",
                CanonicalScalar::Duration(CanonicalDuration::new(14, -3, 5).unwrap()),
            ),
        ];
        for (expression, expected) in cases {
            let statement = format!("MATCH (n:Person) SET n.d = {expression} RETURN n.d AS d");
            let result = db
                .query_write(
                    txn,
                    query,
                    commit,
                    &statement,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    write_policy(),
                    |_| Err::<ElementId, _>("SET must not allocate identities"),
                )
                .await
                .unwrap();
            assert_eq!(
                result,
                rows(
                    &["d"],
                    vec![
                        vec![scalar(expected.clone())],
                        vec![scalar(expected.clone())]
                    ],
                ),
                "{expression}"
            );
            for id in [VId(1), VId(2)] {
                let vertex = db.vertex(id).unwrap().unwrap();
                let stored = vertex.props.iter().find(|(key, _)| *key == D);
                assert_eq!(
                    stored.map(|(_, value)| value),
                    Some(&expected.clone().into()),
                    "{expression}"
                );
            }
        }
    });
}

struct ZonedResolver(GqlTimeZones);
impl GraphSymbolResolver for ZonedResolver {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }
    fn time_zones(&self) -> Option<GqlTimeZones> {
        Some(self.0.clone())
    }
}

fn artifact(offset: i32) -> Arc<PinnedTzdb> {
    Arc::new(
        PinnedTzdb::new(vec![TzdbZone {
            identifier: "Test/Fixed".to_owned(),
            initial_offset_seconds: offset,
            transitions: vec![],
        }])
        .unwrap(),
    )
}

fn resolver(artifact: Arc<PinnedTzdb>) -> ZonedResolver {
    let id = artifact.object_id();
    ZonedResolver(GqlTimeZones::new(artifact, id).unwrap())
}

#[test]
fn named_zone_set_requires_and_retains_the_exact_pinned_artifact() {
    run(async |commit, query, txn| {
        let pinned = artifact(3600);
        let mut db = Database::open_memory(commit, keys().with_scalar_resolver(pinned.clone()))
            .await
            .unwrap();
        db.write(commit, fixture()).await.unwrap();
        let statement = "MATCH (n:Person) SET n.d = \
                         datetime({year: 2024, month: 1, day: 1, timezone: 'Test/Fixed'})";
        let bound = |zones| {
            PreparedGraphMutationText::prepare(statement, R, resolver(zones))
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap()
                .canonical_bytes()
        };
        assert_eq!(bound(pinned.clone()), bound(artifact(3600)));
        assert_ne!(bound(pinned.clone()), bound(artifact(7200)));
        let before = db.frontier().unwrap();
        let returned = format!("{statement} RETURN n.d AS d");
        let error = db
            .query_write(
                txn,
                query,
                commit,
                &returned,
                &GqlParameters::new(),
                symbols,
                R,
                write_policy(),
                |_| Err::<ElementId, _>("SET must not allocate identities"),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("TemporalConstruction"),
            "{error}"
        );
        assert_eq!(db.frontier().unwrap(), before);
        let result = db
            .query_write(
                txn,
                query,
                commit,
                &returned,
                &GqlParameters::new(),
                resolver(pinned.clone()),
                R,
                write_policy(),
                |_| Err::<ElementId, _>("SET must not allocate identities"),
            )
            .await
            .unwrap();
        // 2024-01-01T00:00:00 at +01:00 is 2023-12-31T23:00:00Z.
        let expected = scalar(CanonicalScalar::Timestamp(
            CanonicalTimestamp::zoned(
                1_704_063_600_000_000_000,
                3600,
                "Test/Fixed",
                pinned.object_id(),
                pinned.as_ref(),
            )
            .unwrap(),
        ));
        assert_eq!(
            result,
            rows(&["d"], vec![vec![expected.clone()], vec![expected]])
        );
    });
}
