//! A host may let several Warden charges share one sample of the clock it
//! supplies (`QueryCx::with_host_clock_interval`, fgdb-er71y): fgdbd's wall
//! clock cost more than the charges it timed. These laws pin what pacing
//! keeps. Every execution samples on its first charge and on every delivery;
//! at most `interval` charges share a sample; every sample observes expiry,
//! with nothing evaluated after it; and the answer is the per-charge clock's.
//! (Retirement, limits and cancellation do not read the clock; the existing
//! authorized laws keep pinning them on every charge.)
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_prism::{FnxCallSpec, FnxMeter, FnxParameters, FnxReadOptions};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Scope};
use std::num::NonZeroU32;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([9; 32]);
const BRANCH: &str = "host-selected-branch";
/// Clock readings: 100 is inside the grant's validity, 1000 is past it.
const LIVE: u64 = 100;
const EXPIRED: u64 = 1000;

fn authority(seed: u64) -> Authority {
    Authority::new(
        AuthKey::from_seed(seed),
        NS,
        "host-graph",
        SchemaEpoch(0),
        1,
    )
    .unwrap()
}

fn grant() -> Grant {
    let mut grant = Grant::read_only(
        BRANCH,
        EXPIRED,
        QueryLimits {
            max_nodes: 1_000_000,
            max_work: 100_000_000,
            max_rows: 1_000_000,
        },
    );
    grant.labels = Scope::All;
    grant.relations = Scope::All;
    grant.properties = Scope::All;
    grant
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1_000_000, 100_000_000, 100_000_000)
}

fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name| match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .with_duplicates()
}

/// A chain of `n` labelled vertices, 0 -> 1 -> ... -> n-1.
async fn chain(cx: &CommitCx, n: u64) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x51; 32], NS, [0x52; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for at in 0..n {
        batch.create_vertex(
            VId(u128::from(at)),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(at as i64))],
        );
    }
    for at in 1..n {
        batch.add_edge(
            EId(u128::from(10_000 + at)),
            VId(u128::from(at - 1)),
            VId(u128::from(at)),
            vec![],
        );
    }
    db.write(cx, batch).await.unwrap();
    db
}

fn interval(charges: u32) -> NonZeroU32 {
    NonZeroU32::new(charges).unwrap()
}

/// Run `pattern` under `cx`, counting host clock samples; `reading` maps the
/// one-based sample ordinal to the time the clock returns.
fn sampled(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    issuer: &Authority,
    pattern: &PreparedGraphPattern<GraphValueRow>,
    mut reading: impl FnMut(usize) -> u64,
) -> (Result<Vec<GraphValueRow>, QueryError>, usize) {
    let token = issuer.issue_at(&grant(), LIVE).unwrap();
    let mut samples = 0;
    let result =
        db.execute_graph_pattern_authorized(cx, issuer, &token, BRANCH, pattern, policy(), || {
            samples += 1;
            reading(samples)
        });
    (result, samples)
}

#[test]
fn paced_samples_bound_the_charges_they_cover_and_each_observes_expiry() {
    let ((), report) = run_async_under_lab(0x5ec0_e701, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = chain(&contexts.commit(), 400).await;
        let issuer = authority(701);
        let p = pattern("MATCH (a:L)-[:R]->(b) RETURN a, b.p AS p");

        let (expected, per_charge) = sampled(&db, &cx, &issuer, &p, |_| LIVE);
        let expected = expected.unwrap();
        assert_eq!(expected.len(), 399);
        const K: u32 = 16;
        let paced_cx = cx.with_host_clock_interval(interval(K));
        let (answer, paced) = sampled(&db, &paced_cx, &issuer, &p, |_| LIVE);
        assert_eq!(answer.unwrap(), expected, "pacing never changes the answer");
        // The default samples once per charge: plenty of charges to pace.
        assert!(
            per_charge > 40 * K as usize,
            "{per_charge} samples per charge"
        );
        // At most K charges share a sample, and no more samples are taken than
        // one per K charges plus the authorization, first-charge and delivery
        // samples.
        assert!(
            paced >= per_charge / K as usize && paced <= per_charge.div_ceil(K as usize) + 3,
            "{paced} paced samples for {per_charge} charges at K = {K}"
        );

        // Every sample observes expiry, and nothing samples after it.
        for stop in 1..=paced {
            let (refused, seen) = sampled(&db, &paced_cx, &issuer, &p, |at| {
                if at == stop { EXPIRED } else { LIVE }
            });
            assert!(
                matches!(refused, Err(QueryError::Authorization(Error::Expired))),
                "stop={stop}: {refused:?}"
            );
            assert_eq!(seen, stop, "no evaluation after the expired sample");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn delivery_samples_afresh_however_few_charges_share_the_interval() {
    let ((), report) = run_async_under_lab(0x5ec0_e702, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = chain(&contexts.commit(), 3).await;
        let issuer = authority(702);
        let p = pattern("MATCH (a:L)-[:R]->(b) RETURN a, b.p AS p");
        // An interval far longer than the statement: every charge between the
        // first and the delivery shares the first charge's sample.
        let cx = contexts.query().with_host_clock_interval(interval(1 << 30));
        let (answer, samples) = sampled(&db, &cx, &issuer, &p, |_| LIVE);
        assert_eq!(answer.unwrap().len(), 2);
        assert_eq!(
            samples, 3,
            "authorization, the first charge, and delivery each sample"
        );
        // The capability expires after the first charge's sample: delivery
        // sees it, so no row is released.
        let (refused, seen) = sampled(
            &db,
            &cx,
            &issuer,
            &p,
            |at| {
                if at >= 3 { EXPIRED } else { LIVE }
            },
        );
        assert!(
            matches!(refused, Err(QueryError::Authorization(Error::Expired))),
            "{refused:?}"
        );
        assert_eq!(seen, 3);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Run `text` through an authorized read session (fgdbd's path) under `cx`,
/// counting the host clock samples the statement itself takes.
fn session_samples(db: &Database<MemVfs>, cx: &QueryCx, issuer: &Authority, text: &str) -> usize {
    let (result, samples) = session_sampled(db, cx, issuer, text, |_| LIVE);
    result.unwrap();
    samples
}

fn session_sampled(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    issuer: &Authority,
    text: &str,
    mut reading: impl FnMut(usize) -> u64,
) -> (Result<fgdb::QueryResult, QueryError>, usize) {
    let token = issuer.issue_at(&grant(), LIVE).unwrap();
    let samples = std::cell::Cell::new(0_usize);
    let opened = std::cell::Cell::new(None::<usize>);
    let symbols = |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    };
    let mut session = db
        .authorized_read_session(cx, issuer, &token, BRANCH, symbols, policy(), || {
            samples.set(samples.get() + 1);
            opened
                .get()
                .map_or(LIVE, |offset| reading(samples.get() - offset))
        })
        .unwrap();
    let before = samples.get();
    opened.set(Some(before));
    let result = session.query(cx, text, &GqlParameters::new());
    (result, samples.get() - before)
}

#[test]
fn session_statements_share_paced_samples() {
    let ((), report) = run_async_under_lab(0x5ec0_e703, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = chain(&contexts.commit(), 400).await;
        let issuer = authority(703);
        const K: u32 = 16;
        let paced_cx = cx.with_host_clock_interval(interval(K));
        for text in [
            "MATCH (a:L)-[:R]->(b) RETURN count(*) AS n",
            "MATCH (a:L)-[:R]->(b) RETURN a.p AS p",
        ] {
            let per_charge = session_samples(&db, &cx, &issuer, text);
            let paced = session_samples(&db, &paced_cx, &issuer, text);
            assert!(
                per_charge > 40 * K as usize,
                "{text}: {per_charge} samples per charge"
            );
            assert!(
                paced <= per_charge.div_ceil(K as usize) + 3,
                "{text}: {paced} paced samples for {per_charge} charges at K = {K}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn session_analytics_paces_charges_and_samples_every_prepaid_physical_boundary() {
    let ((), report) = run_async_under_lab(0x5ec0_e704, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = chain(&contexts.commit(), 400).await;
        let issuer = authority(704);
        let call = FnxCallSpec::bind(
            "CALL fnx.weakly_connected_components() YIELD vertex,component",
            &FnxParameters::new(),
        )
        .unwrap();
        let options = FnxReadOptions::whole_graph_for(&call, None);
        let view = db.read_session().unwrap();
        let graph = view
            .prism_projection_at(
                &cx,
                view.frontier(),
                options.selection,
                options.projection,
                options.projection_limits,
                options.source_limits,
            )
            .unwrap();
        // Count physical freshness obligations independently of the host's
        // clock. Logical unit charges and uncharged physical polls are distinct
        // operations in the public kernel contract.
        struct Polls<'a>(&'a std::cell::Cell<usize>);
        impl FnxMeter<()> for Polls<'_> {
            fn unit(&mut self) -> Result<(), ()> {
                Ok(())
            }
            fn poll(&mut self) -> Result<(), ()> {
                self.0.set(self.0.get() + 1);
                Ok(())
            }
        }
        let kernel_polls = std::cell::Cell::new(0);
        let mut cursor = call
            .execute_rows(&graph, options.execution_limits, Polls(&kernel_polls))
            .unwrap();
        let mut pull_polls = 0;
        let mut rows = 0;
        let mut scratch = Vec::new();
        while cursor
            .next_row(&mut scratch, || {
                pull_polls += 1;
                Ok::<_, ()>(())
            })
            .unwrap()
        {
            rows += 1;
        }
        assert_eq!((rows, pull_polls), (400, 400));
        // The host also checks the prepaid pull reservation's end and the
        // procedure wrapper's exhausted drain. Neither spends logical work.
        let fresh = kernel_polls.get() + pull_polls + 2;
        let text = "CALL fnx.weakly_connected_components() YIELD vertex, component \
                    RETURN count(DISTINCT component) AS c";
        let (expected, per_charge) = session_sampled(&db, &cx, &issuer, text, |_| LIVE);
        let expected = expected.unwrap();
        const K: u32 = 16;
        let paced_cx = cx.with_host_clock_interval(interval(K));
        let (result, paced) = session_sampled(&db, &paced_cx, &issuer, text, |_| LIVE);
        assert_eq!(result.unwrap(), expected);
        assert!(per_charge > fresh + 40 * K as usize);
        assert!(
            paced <= (per_charge - fresh).div_ceil(K as usize) + fresh + 3,
            "{paced} samples for {per_charge} charge-clock samples and {fresh} fresh polls"
        );
        let physical_cx = cx.with_host_clock_interval(NonZeroU32::MAX);
        let (result, physical) = session_sampled(&db, &physical_cx, &issuer, text, |_| LIVE);
        assert_eq!(result.unwrap(), expected);
        assert_eq!(
            physical,
            fresh + 3,
            "only authorization, first charge, delivery and independently counted physical polls"
        );
        // No prepaid physical boundary may share away an expired sample, and
        // no source, evaluator or completion callback may run past its refusal.
        for stop in 1..=physical {
            let (result, seen) = session_sampled(&db, &physical_cx, &issuer, text, |at| {
                if at == stop { EXPIRED } else { LIVE }
            });
            assert!(
                matches!(result, Err(QueryError::Authorization(Error::Expired))),
                "sample {stop}: {result:?}"
            );
            assert_eq!(seen, stop, "nothing samples after expiry at {stop}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
