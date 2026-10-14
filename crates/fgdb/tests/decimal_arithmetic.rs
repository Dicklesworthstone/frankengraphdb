//! Decimal expressions through the real capability-scoped text/read/write paths.
//! Literal expected coefficients, not results from the arithmetic under test.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphIntegerError, GraphIntegerErrorKind, GraphSymbol,
    GraphSymbolKind, GraphWriteProgramPolicy,
};
use fgdb_types::{
    CanonicalDecimal, CanonicalF64, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId,
    EmbeddedTxnCompletion, PurposeContexts, QueryCx, VId,
};
use fgdb_warden::{Authority, CapabilityToken, Grant, QueryLimits, Rights, Scope};

const R: RelationId = RelationId(1);
const VISIBLE: LabelId = LabelId(1);
const AMOUNT: PropertyKeyId = PropertyKeyId(1);
const DIVISOR: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xd3; 32]);
const UNIT: i128 = 1_000_000_000_000_000_000;
const FIRST: VId = VId((1_u128 << 96) + 1);
const SECOND: VId = VId((1_u128 << 112) + 2);
const HIDDEN: VId = VId((1_u128 << 120) + 3);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xd2; 32], NS, [0xd4; 32])
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0xd3a1), NS, "graph", SchemaEpoch(1), 1).unwrap()
}

fn grant(rows: u64) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([VISIBLE]),
        relations: Scope::only([R]),
        properties: Scope::only([AMOUNT, DIVISOR]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 1_000_000,
            max_work: 100_000_000,
            max_rows: rows,
        },
        expires_at_ms: 10_000,
    }
}

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 1_000_000),
        100,
        100,
        100,
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(VISIBLE)),
        (GraphSymbolKind::Property, "amount") => Some(GraphSymbol::Property(AMOUNT)),
        (GraphSymbolKind::Property, "divisor") => Some(GraphSymbol::Property(DIVISOR)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}

fn d(coefficient: i128) -> CanonicalScalar {
    CanonicalScalar::Decimal(CanonicalDecimal::from_coefficient(coefficient).unwrap())
}

fn argument(name: &str, coefficient: i128) -> GqlParameters {
    GqlParameters::new()
        .with_scalar(name, d(coefficient))
        .unwrap()
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for (id, amount, divisor, label) in [
        (FIRST, 10 * UNIT + 1, 2 * UNIT, VISIBLE),
        (SECOND, 20 * UNIT + 3, 2 * UNIT, VISIBLE),
        // An invisible zero divisor must not turn the visible query into an error.
        (HIDDEN, fgdb_types::MAX_DECIMAL_COEFFICIENT, 0, LabelId(2)),
    ] {
        batch.create_vertex(
            id,
            vec![label],
            vec![(AMOUNT, d(amount)), (DIVISOR, d(divisor)), (SECRET, d(0))],
        );
    }
    db.write(cx, batch).await.unwrap();
}

fn amount(db: &Database<MemVfs>, vertex: VId) -> CanonicalScalar {
    db.vertex(vertex)
        .unwrap()
        .unwrap()
        .props
        .into_iter()
        .find(|(key, _)| *key == AMOUNT)
        .unwrap()
        .1
}

fn arithmetic(error: &(dyn core::error::Error + 'static)) -> Option<GraphIntegerErrorKind> {
    let mut source = Some(error);
    while let Some(error) = source {
        if let Some(error) = error.downcast_ref::<GraphIntegerError>() {
            return Some(error.kind);
        }
        source = error.source();
    }
    None
}

fn read_amounts(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    issuer: &Authority,
    token: &CapabilityToken,
    text: &str,
    params: &GqlParameters,
) -> Vec<i128> {
    let mut session = db
        .authorized_read_session(
            cx,
            issuer,
            token,
            "main",
            symbols,
            policy().mutations.query,
            || NOW,
        )
        .unwrap();
    let QueryResult::Rows { columns, rows } = session.query(cx, text, params).unwrap() else {
        panic!("read session returned a write");
    };
    assert_eq!(columns, vec!["amount"]);
    rows.iter()
        .map(|row| match row.as_slice() {
            [QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Decimal(value)))] => {
                value.coefficient()
            }
            _ => panic!("decimal expression did not return exactly one decimal cell"),
        })
        .collect()
}

fn lab<F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let ((), report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parameterized_decimal_reads_and_writes_preserve_rounding_masking_and_reopen() {
    lab(0xd3a1_0001, |contexts| async move {
        let (cx, txn, commit) = (contexts.query(), contexts.txn(), contexts.commit());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(10), NOW).unwrap();
        // The text matches every vertex; only capability masking excludes
        // the hidden vertex (whose division would fail).
        let query = "MATCH (n) RETURN n.amount * $factor AS amount ORDER BY amount";
        for (factor, expected) in [
            (UNIT / 2, vec![5 * UNIT, 10 * UNIT + 2]),
            (UNIT / 4, vec![5 * UNIT / 2, 5 * UNIT + 1]),
        ] {
            assert_eq!(
                read_amounts(
                    &db,
                    &cx,
                    &issuer,
                    &token,
                    query,
                    &argument("factor", factor)
                ),
                expected
            );
        }
        assert_eq!(
            read_amounts(
                &db,
                &cx,
                &issuer,
                &token,
                "MATCH (n) RETURN n.amount / n.divisor AS amount ORDER BY amount",
                &GqlParameters::new(),
            ),
            vec![5 * UNIT, 10 * UNIT + 2]
        );
        assert_eq!(
            read_amounts(
                &db,
                &cx,
                &issuer,
                &token,
                "MATCH (n) WHERE n.amount / n.divisor > $threshold \
                 RETURN n.amount AS amount ORDER BY amount",
                &argument("threshold", 6 * UNIT),
            ),
            vec![20 * UNIT + 3]
        );
        let before = db.frontier().unwrap();
        let original_hidden = db.vertex(HIDDEN).unwrap();
        let zero_rows = issuer.issue_at(&grant(0), NOW).unwrap();
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &zero_rows,
                "main",
                symbols,
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let (stats, completion) = session
            .query_stats(
                &cx,
                "MATCH (n) SET n.amount = n.amount * $factor",
                &argument("factor", UNIT / 2),
            )
            .await
            .unwrap();
        assert_eq!(stats.mutation_effects, 2);
        drop(session);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } if commit_seq.0 == before.0 + 1
        ));
        assert_eq!(amount(&db, FIRST), d(5 * UNIT));
        assert_eq!(amount(&db, SECOND), d(10 * UNIT + 2));
        assert_eq!(db.vertex(HIDDEN).unwrap(), original_hidden);
        for vertex in [FIRST, SECOND] {
            assert!(
                db.vertex(vertex)
                    .unwrap()
                    .unwrap()
                    .props
                    .contains(&(SECRET, d(0)))
            );
        }
        let all = db.vertices().unwrap();
        let seq = db.frontier().unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap(), all);
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(
            read_amounts(
                &db,
                &cx,
                &issuer,
                &token,
                "MATCH (n) RETURN n.amount AS amount ORDER BY amount",
                &GqlParameters::new(),
            ),
            vec![5 * UNIT, 10 * UNIT + 2]
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn a_late_decimal_exception_rolls_back_the_entire_program_and_closes_the_session() {
    lab(0xd3a1_0002, |contexts| async move {
        let (cx, txn, commit) = (contexts.query(), contexts.txn(), contexts.commit());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(0), NOW).unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let text = "MATCH (n) SET n.amount = n.amount + $inc; \
                    MATCH (n) SET n.amount = n.amount / $divisor";
        for divisor in [0, 2 * UNIT] {
            let params = argument("inc", UNIT)
                .with_scalar("divisor", d(divisor))
                .unwrap();
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    symbols,
                    R,
                    policy(),
                    64,
                    || NOW,
                )
                .unwrap();
            let result = session.query_stats(&cx, text, &params).await;
            if divisor == 0 {
                let error = result.unwrap_err();
                assert_eq!(
                    arithmetic(&error),
                    Some(GraphIntegerErrorKind::DivisionByZero)
                );
                assert!(session.is_closed());
                drop(session);
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
            } else {
                let (stats, completion) = result.unwrap();
                assert_eq!((stats.completed_statements, stats.mutation_effects), (2, 4));
                drop(session);
                assert!(matches!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted { commit_seq }
                        if commit_seq.0 == (before.0).0 + 1
                ));
                assert_eq!(amount(&db, FIRST), d(5_500_000_000_000_000_000));
                assert_eq!(amount(&db, SECOND), d(10_500_000_000_000_000_002));
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn decimal_math_does_not_relax_numeric_domains_scope_or_where_exceptions() {
    lab(0xd3a1_0003, |contexts| async move {
        let (cx, txn, commit) = (contexts.query(), contexts.txn(), contexts.commit());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(10), NOW).unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let multiply = "MATCH (n) SET n.amount = n.amount * $factor";
        for (text, factor, kind) in [
            (
                multiply,
                CanonicalScalar::Int(2),
                Some(GraphIntegerErrorKind::IncompatibleOperands),
            ),
            (
                multiply,
                CanonicalScalar::Float(CanonicalF64::new(0.5)),
                Some(GraphIntegerErrorKind::IncompatibleOperands),
            ),
            (
                multiply,
                d(fgdb_types::MAX_DECIMAL_COEFFICIENT),
                Some(GraphIntegerErrorKind::Overflow),
            ),
            // Original attempted hidden writes still refuse, even exact no-ops.
            ("MATCH (n) SET n.secret = $factor * $factor", d(0), None),
        ] {
            let params = GqlParameters::new().with_scalar("factor", factor).unwrap();
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    symbols,
                    R,
                    policy(),
                    64,
                    || NOW,
                )
                .unwrap();
            let error = session.query_stats(&cx, text, &params).await.unwrap_err();
            if let Some(kind) = kind {
                assert_eq!(arithmetic(&error), Some(kind));
            }
            assert!(session.is_closed());
            drop(session);
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
        }
        let mut session = db
            .authorized_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols,
                policy().mutations.query,
                || NOW,
            )
            .unwrap();
        assert!(
            session
                .query(
                    &cx,
                    "MATCH (n) WHERE n.amount / $zero > $zero RETURN n.amount AS amount",
                    &argument("zero", 0),
                )
                .is_err(),
            "division failure is not WHERE UNKNOWN"
        );
        drop(session);
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );
        let mut read_only = grant(10);
        read_only.rights = Rights::Read;
        let token = issuer.issue_at(&read_only, NOW).unwrap();
        assert!(
            db.authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                "main",
                symbols,
                R,
                policy(),
                64,
                || NOW,
            )
            .is_err()
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}
