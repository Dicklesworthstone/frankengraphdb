//! fgdb-61p1n: a reused full scan is the scan it replaces. Its live calls
//! replay in order, so rows, every signed refusal point and every
//! cancellation ordinal agree with a fresh scan; statements with one
//! admitted set share it; scoped capabilities never reuse or populate; and a
//! commit never serves the previous generation's records.

use super::super::{SCANS, WORK_RUNS};
use super::{AdmissionKey, INLINE_IDENTITIES, POSITIONED_EDGES};
use crate::{Database, DatabaseKeys, MemVfs, QueryError, QueryResult, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use fgdb_warden::{Authority, CapabilityToken, Grant, QueryLimits, Restriction, Scope};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::Ordering;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x61; 32]);
const BRANCH: &str = "main";
const P: PropertyKeyId = PropertyKeyId(1);
const W: PropertyKeyId = PropertyKeyId(2);
/// Whole-graph statements that read edges: one admitted set serves them all.
const EDGE_READS: [&str; 5] = [
    "MATCH (a:L)-[:R]->(b:L) RETURN count(*) AS n",
    "MATCH (a:L)-[:R]->(b) RETURN a.p AS p, count(*) AS d ORDER BY d DESC, p ASC LIMIT 3",
    "MATCH (a:L)-[:R]->(b:L)-[:R]->(c:L) RETURN count(*) AS n",
    "MATCH (a:L)-[:R]->(b:M) RETURN b.p AS p ORDER BY p",
    // Each edge's own property beside its endpoints: a refill that pairs a
    // record's properties with another record's identity answers differently.
    "MATCH (a)-[r:R]->(b) RETURN a.p AS p, b.p AS q, r.w AS w ORDER BY p, q, w",
];
/// A vertex-only plan admits no edge: its own admitted set.
const VERTEX_READ: &str = "MATCH (n:L) RETURN count(*) AS n";

fn scans() -> usize {
    SCANS.with(Cell::get)
}

/// These tests vary process-wide physical-profile instrumentation.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(0x61), NS, "graph", SchemaEpoch(0), 1).unwrap()
}

/// Every label, relation and property: the scope that reuses.
fn open() -> Grant {
    let mut grant = Grant::read_only(
        BRANCH,
        10_000,
        QueryLimits {
            max_nodes: 10_000,
            max_work: 10_000_000,
            max_rows: 10_000,
        },
    );
    grant.labels = Scope::All;
    grant.relations = Scope::All;
    grant.properties = Scope::All;
    grant
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 10_000, 10_000_000, 1_000_000)
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "M") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
        _ => None,
    }
}

fn weight(value: i64) -> Vec<(PropertyKeyId, CanonicalScalar)> {
    vec![(W, CanonicalScalar::Int(value))]
}

/// Four commits over two relations: multi-label vertices with a property, a
/// self loop, a parallel pair, a deleted edge, a relabelled (re-versioned)
/// vertex and a vertex born and deleted later, so both history merges poll
/// superseded and retired statements between admissions.
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x62; 32], NS, [0x63; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 1..=8u128 {
        let labels = match id {
            3 => vec![LabelId(1), LabelId(2), LabelId(3)],
            5 | 6 => vec![LabelId(1), LabelId(2)],
            _ => vec![LabelId(1)],
        };
        batch.create_vertex(
            VId(id),
            labels,
            vec![(P, CanonicalScalar::Int(id as i64 * 10))],
        );
    }
    for (eid, source, target, value) in [
        (1, 1, 2, 3),
        (2, 2, 3, 1),
        (3, 3, 4, 4),
        (4, 4, 5, 1),
        (5, 5, 5, 9),
        (6, 1, 2, 2),
        (7, 6, 7, 5),
        (8, 7, 1, 6),
    ] {
        batch.add_edge(EId(eid), VId(source), VId(target), weight(value));
    }
    db.write(cx, batch).await.unwrap();
    let mut other = WriteBatch::new(RelationId(2));
    other.create_vertex(VId(9), vec![LabelId(2)], vec![]);
    for (eid, source, target) in [(20, 8, 9), (21, 9, 3), (22, 2, 6)] {
        other.add_edge(EId(eid), VId(source), VId(target), vec![]);
    }
    db.write(cx, other).await.unwrap();
    let mut changes = WriteBatch::new(RelationId(1));
    changes.delete_edge(EId(2));
    changes.set_vertex_label(VId(4), LabelId(2), true);
    changes.add_edge(EId(9), VId(8), VId(4), weight(7));
    db.write(cx, changes).await.unwrap();
    let mut retire = WriteBatch::new(RelationId(2));
    retire.delete_edge(EId(20));
    retire.delete_edge(EId(21));
    retire.delete_vertex(VId(9));
    db.write(cx, retire).await.unwrap();
    db
}

fn change() -> WriteBatch {
    let mut change = WriteBatch::new(RelationId(1));
    change.add_edge(EId(1000), VId(2), VId(3), weight(1));
    change.add_edge(EId(1001), VId(3), VId(6), weight(2));
    change.delete_edge(EId(8));
    change.create_vertex(
        VId(40),
        vec![LabelId(1)],
        vec![(P, CanonicalScalar::Int(5))],
    );
    change
}

fn read(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    token: &CapabilityToken,
    text: &str,
    clock: impl FnMut() -> u64,
) -> Result<QueryResult, QueryError> {
    db.query_authorized(
        cx,
        &issuer(),
        token,
        BRANCH,
        text,
        &GqlParameters::new(),
        symbols,
        policy(),
        clock,
    )
}

/// One statement's observable live footprint: rows (or the refusal),
/// query-context checkpoints and host clock reads.
fn footprint(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    token: &CapabilityToken,
    text: &str,
) -> (String, usize, usize) {
    let probe = Arc::new(SimulationCheckpointProbe::new(None));
    let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
    let reads = Cell::new(0);
    let result = read(db, &observed, token, text, || {
        reads.set(reads.get() + 1);
        100
    });
    (format!("{result:?}"), probe.calls(), reads.get())
}

/// The refill forms a core can take: inline identities or not, positioned
/// edges or not.
const FORMS: [(bool, bool); 4] = [(true, true), (false, true), (true, false), (false, false)];

fn with_forms(law: impl Fn(bool, bool)) {
    for (inline, positioned) in FORMS {
        INLINE_IDENTITIES.store(inline, Ordering::Relaxed);
        POSITIONED_EDGES.store(positioned, Ordering::Relaxed);
        law(inline, positioned);
    }
    INLINE_IDENTITIES.store(true, Ordering::Relaxed);
    POSITIONED_EDGES.store(true, Ordering::Relaxed);
}

/// A hit spends what the scan spent: equal rows, query-context checkpoints
/// and clock reads, and every checkpoint ordinal interrupts the hit there,
/// whichever form its core takes.
#[test]
fn a_reused_full_scan_spends_exactly_what_its_scan_spent() {
    let _serial = serial();
    with_forms(a_reused_full_scan_spends_exactly_what_its_scan_spent_with);
}

fn a_reused_full_scan_spends_exactly_what_its_scan_spent_with(inline: bool, positioned: bool) {
    let ((), report) = run_async_under_lab(0x61b1_0001, move |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let token = issuer().issue_at(&open(), 100).unwrap();
        for text in EDGE_READS.into_iter().chain([VERTEX_READ]) {
            let db = database(&c.commit()).await;
            let before = scans();
            let cold = footprint(&db, &cx, &token, text);
            assert_eq!(scans(), before + 1, "{text}: the first statement scans");
            assert!(cold.0.starts_with("Ok("), "{text}: {cold:?}");
            let warm = footprint(&db, &cx, &token, text);
            assert_eq!(scans(), before + 1, "{text}: the second statement reuses");
            assert_eq!(warm, cold, "{text}");
            if text != VERTEX_READ {
                let at = db.frontier().unwrap();
                let core = db
                    .snapshot
                    .read_caches
                    .admitted
                    .lookup(&db.snapshot, AdmissionKey { at, edges: true })
                    .unwrap();
                assert_eq!(core.identities.is_some(), inline, "{text}: refill form");
                assert_eq!(
                    core.positioned.is_some(),
                    positioned,
                    "{text}: positioned form"
                );
            }
            // Interrupted at any ordinal, the hit stops where a scan on a
            // database with nothing to reuse stops, with the same refusal.
            let interrupted = |db: &Database<MemVfs>, stop| {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let result = read(db, &controlled, &token, text, || 100);
                // The injected error carries a per-interruption correlation
                // id, so its kind is compared, not its text.
                assert!(
                    matches!(
                        result,
                        Err(QueryError::Pattern(fgdb_gql::GqlQueryError::Interrupted(_)))
                    ),
                    "{text} stop {stop}: {result:?}"
                );
                probe.calls()
            };
            for stop in 1..=cold.1 {
                let before = scans();
                let hit = interrupted(&db, stop);
                assert_eq!(scans(), before, "{text} stop {stop}: the hit scanned");
                let fresh = database(&c.commit()).await;
                assert_eq!(hit, interrupted(&fresh, stop), "{text} stop {stop}");
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Different statements over one cut share one admitted set, and each spends
/// on a warm database exactly what it spends alone on a fresh one.
#[test]
fn statements_with_one_admitted_set_share_it() {
    let _serial = serial();
    with_forms(statements_with_one_admitted_set_share_it_with);
}

fn statements_with_one_admitted_set_share_it_with(inline: bool, positioned: bool) {
    let ((), report) = run_async_under_lab(0x61b1_0002, move |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let token = issuer().issue_at(&open(), 100).unwrap();
        let warm = database(&c.commit()).await;
        let before = scans();
        read(&warm, &cx, &token, EDGE_READS[0], || 100).unwrap();
        assert_eq!(scans(), before + 1);
        for text in EDGE_READS {
            let before = scans();
            let shared = footprint(&warm, &cx, &token, text);
            assert_eq!(scans(), before, "{text} scanned again");
            let alone = database(&c.commit()).await;
            assert_eq!(footprint(&alone, &cx, &token, text), shared, "{text}");
            assert_eq!(scans(), before + 1, "{text}: the fresh database scanned");
        }
        // A vertex-only plan admits no edge, so it is a different set.
        let before = scans();
        read(&warm, &cx, &token, VERTEX_READ, || 100).unwrap();
        assert_eq!(scans(), before + 1);
        read(&warm, &cx, &token, VERTEX_READ, || 100).unwrap();
        assert_eq!(scans(), before + 1);
        // Both sets are retained, as coordinates of every admitted record.
        let at = warm.frontier().unwrap();
        let edges = warm
            .snapshot
            .read_caches
            .admitted
            .lookup(&warm.snapshot, AdmissionKey { at, edges: true })
            .unwrap();
        let vertices = warm
            .snapshot
            .read_caches
            .admitted
            .lookup(&warm.snapshot, AdmissionKey { at, edges: false })
            .unwrap();
        assert_eq!((edges.vertices.len(), vertices.vertices.len()), (8, 8));
        assert_eq!((edges.edges.len(), vertices.edges.len()), (9, 0));
        assert_eq!((edges.records, vertices.records), (17, 8));
        assert_eq!(
            edges.identities.as_ref().map(Vec::len),
            inline.then_some(9),
            "edge identities inline only while they fit"
        );
        // Positioned edges name each admitted edge's endpoints in the sealed
        // vertex table, in identity order, beside its relation.
        assert_eq!(
            edges.positioned.as_ref().map(|form| form.edges.len()),
            positioned.then_some(9),
            "edges positioned only while they fit"
        );
        if let (Some(form), Some(identities)) = (&edges.positioned, &edges.identities) {
            let mut sealed: Vec<VId> = edges.vertices.iter().map(|&(vid, ..)| vid).collect();
            sealed.sort_unstable();
            sealed.dedup();
            for (edge, &(_, source, relation, destination)) in form.edges.iter().zip(identities) {
                assert_eq!(sealed[edge.source as usize], source);
                assert_eq!(sealed[edge.destination as usize], destination);
                assert_eq!(form.relations[edge.relation as usize], relation);
            }
        }
        assert!(
            vertices.positioned.is_none(),
            "a vertex-only core has no edges"
        );
        // The reused statements above resolved vertices through the core's
        // position index, so their equality with the fresh scans covers it.
        for core in [&edges, &vertices] {
            assert!(
                core.positions.get().is_some_and(Option::is_some),
                "a reused core indexes its sealed vertex table"
            );
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// The signed MaxWork and MaxNodes a holder may attenuate refuse a hit at
/// the same threshold, with the same refusal, as a fresh scan on a database
/// that has never reused anything.
#[test]
fn signed_refusal_points_of_a_hit_match_a_fresh_scan() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x61b1_0003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let warm = database(&c.commit()).await;
        let token = issuer().issue_at(&open(), 100).unwrap();
        let limits: [fn(u64) -> Restriction; 2] = [Restriction::MaxWork, Restriction::MaxNodes];
        for text in [EDGE_READS[0], EDGE_READS[1], VERTEX_READ] {
            read(&warm, &cx, &token, text, || 100).unwrap();
            for limit in limits {
                let attenuated = |value| token.attenuate(limit(value)).unwrap();
                let before = scans();
                // Smallest admitting limit on the warm (reusing) database.
                let (mut low, mut high) = (0, 10_000_000);
                while low < high {
                    let middle = low + (high - low) / 2;
                    if read(&warm, &cx, &attenuated(middle), text, || 100).is_ok() {
                        high = middle;
                    } else {
                        low = middle + 1;
                    }
                }
                assert_eq!(scans(), before, "{text}: every probe reused");
                assert!(low > 0);
                let warm_refusal = footprint(&warm, &cx, &attenuated(low - 1), text);
                assert!(warm_refusal.0.starts_with("Err("), "{warm_refusal:?}");
                // Fresh databases have nothing to reuse. A refusal after
                // admission may retain it, so the admitted statement gets its
                // own fresh database too.
                let cold = database(&c.commit()).await;
                assert_eq!(
                    footprint(&cold, &cx, &attenuated(low - 1), text),
                    warm_refusal,
                    "{text} at {}",
                    low - 1
                );
                let cold = database(&c.commit()).await;
                let before = scans();
                let admitted = footprint(&cold, &cx, &attenuated(low), text);
                assert_eq!(scans(), before + 1, "{text}: the cold statement scanned");
                assert_eq!(footprint(&warm, &cx, &attenuated(low), text), admitted);
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Signed work charged in runs (fgdb-62mzm) admits and refuses native reads
/// exactly where charging every unit alone does: for MaxWork, MaxNodes and
/// MaxRows, the same smallest admitting limit, the same refusal one below it
/// and the same admitted rows, with no more query-context polls.
#[test]
fn work_runs_admit_and_refuse_native_reads_at_the_per_unit_thresholds() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x61b1_0009, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let token = issuer().issue_at(&open(), 100).unwrap();
        let limits: [fn(u64) -> Restriction; 3] = [
            Restriction::MaxWork,
            Restriction::MaxNodes,
            Restriction::MaxRows,
        ];
        for text in EDGE_READS.into_iter().chain([VERTEX_READ]) {
            for limit in limits {
                let threshold = |runs: bool| {
                    WORK_RUNS.with(|cell| cell.set(runs));
                    let attenuated = |value| token.attenuate(limit(value)).unwrap();
                    let (mut low, mut high) = (0, 10_000_000);
                    while low < high {
                        let middle = low + (high - low) / 2;
                        if read(&db, &cx, &attenuated(middle), text, || 100).is_ok() {
                            high = middle;
                        } else {
                            low = middle + 1;
                        }
                    }
                    let refused =
                        (low > 0).then(|| footprint(&db, &cx, &attenuated(low - 1), text));
                    let admitted = footprint(&db, &cx, &attenuated(low), text);
                    WORK_RUNS.with(|cell| cell.set(true));
                    (low, refused, admitted)
                };
                let (unit_low, unit_refused, unit_admitted) = threshold(false);
                let (run_low, run_refused, run_admitted) = threshold(true);
                assert_eq!(run_low, unit_low, "{text}: admitting limit");
                assert_eq!(
                    run_refused.map(|footprint| footprint.0),
                    unit_refused.map(|footprint| footprint.0),
                    "{text}: refusal"
                );
                assert_eq!(run_admitted.0, unit_admitted.0, "{text}: rows");
                assert!(run_admitted.1 <= unit_admitted.1, "{text}: polls");
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A capability with any label, relation or property scope scans for itself
/// every time: it never sees the unrestricted set cached before it, and
/// leaves nothing for an unrestricted caller. A database that never cached
/// anything is the oracle for its rows.
#[test]
fn scoped_capabilities_never_reuse_or_populate() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x61b1_0004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let pristine = database(&c.commit()).await;
        let open = issuer().issue_at(&open(), 100).unwrap();
        let unrestricted: Vec<_> = EDGE_READS
            .iter()
            .map(|text| format!("{:?}", read(&db, &cx, &open, text, || 100)))
            .collect();
        for restriction in [
            Restriction::Labels(Scope::only([LabelId(2)])),
            Restriction::Relations(Scope::only([RelationId(2)])),
            Restriction::Properties(Scope::only([W])),
            Restriction::DenyProperties([P].into()),
        ] {
            let token = open.attenuate(restriction.clone()).unwrap();
            let mut masked = false;
            for (text, unrestricted) in EDGE_READS.iter().zip(&unrestricted) {
                let before = scans();
                let first = format!("{:?}", read(&db, &cx, &token, text, || 100));
                assert_eq!(
                    format!("{:?}", read(&pristine, &cx, &token, text, || 100)),
                    first,
                    "{restriction:?} saw another scope's records: {text}"
                );
                let second = format!("{:?}", read(&db, &cx, &token, text, || 100));
                assert_eq!(first, second);
                assert_eq!(scans(), before + 3, "{restriction:?} reused: {text}");
                masked |= *unrestricted != first;
            }
            assert!(masked, "{restriction:?} must change some rows");
        }
        let before = scans();
        read(&pristine, &cx, &open, EDGE_READS[0], || 100).unwrap();
        assert_eq!(scans(), before + 1, "a scoped scan populated the cache");
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A commit after a reuse yields the new generation's rows, equal to a
/// database that never cached. A view pinning the old generation keeps it
/// alive, so the commit copies the snapshot and the cached set stays with the
/// old allocation.
#[test]
fn a_commit_never_serves_the_previous_generation() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x61b1_0005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let token = issuer().issue_at(&open(), 100).unwrap();
        for text in EDGE_READS.into_iter().chain([VERTEX_READ]) {
            for pin in [false, true] {
                let mut db = database(&c.commit()).await;
                let old = read(&db, &cx, &token, text, || 100).unwrap();
                let before = scans();
                assert_eq!(read(&db, &cx, &token, text, || 100).unwrap(), old);
                assert_eq!(scans(), before, "{text}: reused before the commit");
                let pinned = pin.then(|| db.read_session().unwrap());
                db.write(&c.commit(), change()).await.unwrap();
                let new = read(&db, &cx, &token, text, || 100).unwrap();
                assert_eq!(scans(), before + 1, "{text}: the new generation scans");
                assert_ne!(new, old, "{text}: the change must move the rows");
                let mut oracle = database(&c.commit()).await;
                oracle.write(&c.commit(), change()).await.unwrap();
                assert_eq!(read(&oracle, &cx, &token, text, || 100).unwrap(), new);
                drop(pinned);
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Over a single-version graph the retained tape is a handful of runs however
/// many records it admits, and the privileged read agrees with the reuse.
#[test]
fn a_single_version_scan_retains_a_constant_size_tape() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x61b1_0006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db =
            Database::open_memory(&c.commit(), DatabaseKeys::new([0x62; 32], NS, [0x63; 32]))
                .await
                .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 0..400u128 {
            batch.create_vertex(VId(id), vec![LabelId(1)], vec![]);
        }
        for id in 0..400u128 {
            batch.add_edge(EId(id), VId(id), VId((id * 7 + 1) % 400), weight(1));
        }
        db.write(&c.commit(), batch).await.unwrap();
        let token = issuer().issue_at(&open(), 100).unwrap();
        let text = EDGE_READS[0];
        let first = read(&db, &cx, &token, text, || 100).unwrap();
        assert_eq!(read(&db, &cx, &token, text, || 100).unwrap(), first);
        assert_eq!(
            first,
            db.query(&cx, text, &GqlParameters::new(), symbols, policy())
                .unwrap()
        );
        let at = db.frontier().unwrap();
        let core = db
            .snapshot
            .read_caches
            .admitted
            .lookup(&db.snapshot, AdmissionKey { at, edges: true })
            .unwrap();
        assert_eq!((core.vertices.len(), core.edges.len()), (400, 400));
        assert!(core.tape.runs().count() <= 12, "{:?}", core.tape);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A repeated filterless count neither merges history nor refills the vertex
/// table. A refill initializes the core's shared vertex-position cache, even
/// when its small-table result is None, so an untouched OnceLock distinguishes
/// the summary path from merely reusing admission before traversing rows.
#[test]
fn filterless_counts_replay_without_refilling_source_tables() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x61b1_0007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let token = issuer().issue_at(&open(), 100).unwrap();
        for text in [
            "MATCH (n) RETURN count(*) AS n",
            "MATCH (a)-[:R]->(b) RETURN count(*) AS n",
        ] {
            let db = database(&c.commit()).await;
            let cold = read(&db, &cx, &token, text, || 100).unwrap();
            let before = scans();
            let core = db
                .snapshot
                .read_caches
                .admitted
                .lookup(
                    &db.snapshot,
                    AdmissionKey {
                        at: db.frontier().unwrap(),
                        edges: text.contains("[:R]"),
                    },
                )
                .unwrap();
            assert!(core.positions.get().is_none());
            assert_eq!(read(&db, &cx, &token, text, || 100).unwrap(), cold);
            assert_eq!(scans(), before);
            assert!(core.positions.get().is_none(), "{text}: table was refilled");
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Many relationship types cannot grow an unbounded summary table. Its fixed
/// capacity is accounted in the existing core, and exceeding it chooses the
/// ordinary edge visitor while retaining the exact vertex cardinality.
#[test]
fn relation_count_summaries_are_bounded_and_accounted() {
    use super::{COUNT_SUMMARY_RELATIONS, Recording};
    use fgdb_gql::GraphCountSource;
    let _serial = serial();
    for relations in [1, COUNT_SUMMARY_RELATIONS, COUNT_SUMMARY_RELATIONS + 1] {
        let mut recording = Recording::default();
        recording.vertex(VId(1), 0, 0, 0);
        for relation in 0..relations {
            recording.edge(
                (
                    EId(relation as u128),
                    VId(1),
                    RelationId(relation as u64),
                    VId(1),
                ),
                0,
                relation,
                [0, 0],
            );
        }
        let mut core = recording.finish(relations as u64 + 1).unwrap();
        assert_eq!(core.count_source(GraphCountSource::Vertices), Some((1, 1)));
        assert_eq!(
            core.count_source(GraphCountSource::Label(fgdb_delta_types::LabelId(1))),
            None,
            "scoped admission cannot substitute owner label metadata"
        );
        let expected = (relations <= COUNT_SUMMARY_RELATIONS).then_some((relations as u64, 1));
        assert_eq!(
            core.count_source(GraphCountSource::Relation(RelationId(0))),
            expected
        );
        let before = core.bytes();
        let counts = core.positioned.as_mut().unwrap().counts.take();
        let bytes = counts
            .as_ref()
            .map_or(0, |counts| counts.capacity() * std::mem::size_of::<u64>());
        assert_eq!(before - core.bytes(), bytes);
        assert!(bytes <= COUNT_SUMMARY_RELATIONS * std::mem::size_of::<u64>());
    }
}
