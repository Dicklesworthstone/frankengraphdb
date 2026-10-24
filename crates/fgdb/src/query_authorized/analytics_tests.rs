//! fgdb-0tbrz: a reused authorized projection is the build it replaces. The
//! build's live calls replay in order, so rows, every signed refusal point and
//! every cancellation ordinal agree with a fresh build. Scoped capabilities
//! reuse only their exact compiled predicates, and a commit never serves the
//! previous generation's graph.

use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbolKind};
use fgdb_prism::{
    FnxExecutionLimits, FnxSelection, FnxSourceLimits, FnxWeightSpec, MissingWeightPolicy,
    ProjectionLimits, ProjectionSpec,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
};
use fgdb_warden::{Grant, QueryLimits, Restriction, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x0b; 32]);
const BRANCH: &str = "main";
const W: PropertyKeyId = PropertyKeyId(1);
const CALLS: [(&str, Directedness); 3] = [
    (
        "CALL fnx.pagerank(0.85,100,1e-9,true) YIELD vertex,score",
        Directedness::Directed,
    ),
    (
        "CALL fnx.connected_components() YIELD vertex,component",
        Directedness::Undirected,
    ),
    (
        "CALL fnx.single_source_shortest_path_length(1) YIELD vertex,distance",
        Directedness::Reversed,
    ),
];

fn builds() -> usize {
    BUILDS.with(Cell::get)
}

/// The reuse witnesses share the process-wide sidecar: a contended lookup
/// lawfully builds, so the tests that count builds take turns, together with
/// the session tests that fill it.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    super::reuse_test_turn()
}

fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(0x0b), NS, "graph", SchemaEpoch(0), 1).unwrap()
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

fn options(direction: Directedness) -> FnxReadOptions {
    FnxReadOptions {
        as_of: None,
        selection: FnxSelection {
            vertex_label: None,
            relation: None,
            weight: FnxWeightSpec::Property {
                key: W,
                missing: MissingWeightPolicy::Zero,
            },
        },
        projection: ProjectionSpec {
            directedness: direction,
            parallel_edges: ParallelEdgePolicy::Minimum,
            self_loops: SelfLoopPolicy::Keep,
        },
        source_limits: FnxSourceLimits {
            max_work_units: 100_000,
            max_scratch_entries: 10_000,
            max_staging_bytes: 1 << 20,
        },
        projection_limits: ProjectionLimits {
            max_vertices: 1_000,
            max_input_edges: 1_000,
            max_adjacency_entries: 2_000,
            max_workspace_bytes: 1 << 20,
        },
        execution_limits: FnxExecutionLimits {
            max_iterations: 1_000,
            max_result_rows: 1_000,
            max_estimated_work: 1 << 26,
        },
    }
}

#[test]
fn scoped_entries_charge_key_capacity_and_keep_weak_exact_source_ownership() {
    let options = options(Directedness::Directed);
    let request = ProjectionRequest {
        binding: SnapshotBinding {
            root: fgdb_types::ObjectId([71; 32]),
            as_of: CommitSeq(1),
        },
        selection: options.selection,
        projection: options.projection,
        projection_limits: options.projection_limits,
        source_limits: options.source_limits,
    };
    let graph = SnapshotGraphView::build(
        request.binding,
        &[VId(1), VId(2)],
        &[ProjectionEdge {
            eid: EId(1),
            source: VId(1),
            target: VId(2),
            weight: 1.0,
        }],
        request.projection,
        request.projection_limits,
    )
    .unwrap();
    let key = || {
        let mut predicates = Vec::with_capacity(512);
        predicates.extend_from_slice(&[1, 2, 3]);
        ProjectionKey {
            request,
            predicates: Some(predicates),
        }
    };
    let tape = || {
        let mut tape = Tape::bounded(512);
        tape.poll();
        tape.emit(Some(Step::Edge));
        tape.finish()
    };
    let bytes = graph.charged_resident_bytes() + tape().bytes() + key().retained_bytes();
    for allowance in [bytes - 1, bytes] {
        let cache = LiveCache::new(allowance);
        let owner = Arc::new(());
        remember_projection(&cache, &owner, key(), &graph, tape());
        assert_eq!(cache.lookup(&owner, key()).is_some(), allowance == bytes);
    }
    let cache = LiveCache::new(bytes);
    let owner = Arc::new(());
    let weak = Arc::downgrade(&owner);
    remember_projection(&cache, &owner, key(), &graph, tape());
    let same = key();
    let stored = cache.lookup(&owner, &same).unwrap();
    assert!(stored.0.shares_cache_with(&graph));
    let mut changed = key();
    changed.predicates = Some(vec![1, 2, 4]);
    assert!(cache.lookup(&owner, &changed).is_none());
    changed.predicates = None;
    assert!(cache.lookup(&owner, &changed).is_none());
    changed = key();
    changed.request.binding.as_of = CommitSeq(0);
    assert!(cache.lookup(&owner, &changed).is_none());
    assert!(cache.lookup(&Arc::new(()), &same).is_none());
    // A different live key cannot displace or undercharge the retained key.
    let mut changed = key();
    changed.predicates = Some(vec![9]);
    remember_projection(&cache, &owner, changed, &graph, tape());
    assert!(cache.lookup(&owner, &same).is_some());
    assert_eq!(Arc::strong_count(&owner), 1);
    drop(owner);
    assert!(weak.upgrade().is_none());
    let replacement = Arc::new(());
    assert!(cache.lookup(&replacement, &same).is_none());
    remember_projection(&cache, &replacement, key(), &graph, tape());
    assert!(cache.lookup(&replacement, &same).is_some());

    for invalid in [0, 1, 2] {
        let cache = LiveCache::new(bytes);
        let mut candidate = key();
        let mut replay = tape();
        match invalid {
            0 => replay = Tape::bounded(0).finish(),
            1 => candidate.request.binding.root = fgdb_types::ObjectId([72; 32]),
            _ => candidate.request.projection.directedness = Directedness::Reversed,
        }
        remember_projection(&cache, &replacement, candidate, &graph, replay);
        assert!(cache.lookup(&replacement, &same).is_none());
        // A rejected entry has consumed no cache allowance.
        remember_projection(&cache, &replacement, key(), &graph, tape());
        assert!(cache.lookup(&replacement, &same).is_some());
    }
}

fn weight(value: i64) -> Vec<(PropertyKeyId, CanonicalScalar)> {
    vec![(W, CanonicalScalar::Int(value))]
}

/// Four commits over two relations: multi-label vertices, a self loop, a
/// parallel pair, a deleted edge, a relabelled (re-versioned) vertex and a
/// vertex born and deleted later, so the source walk polls superseded and
/// retired history between admissions.
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x0c; 32], NS, [0x0d; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 1..=8u128 {
        let labels = match id {
            3 => vec![LabelId(1), LabelId(2), LabelId(3)],
            5 | 6 => vec![LabelId(1), LabelId(2)],
            _ => vec![LabelId(1)],
        };
        batch.create_vertex(VId(id), labels, vec![]);
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

fn call(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    token: &CapabilityToken,
    text: &str,
    options: FnxReadOptions,
    clock: impl FnMut() -> u64,
) -> Result<Vec<Vec<FnxValue>>, Error> {
    db.call_fnx_authorized(
        cx,
        &issuer(),
        token,
        BRANCH,
        text,
        &FnxParameters::new(),
        options,
        clock,
    )
}

/// One call's observable live footprint: rows (or the refusal), query-context
/// checkpoints and host clock reads.
fn footprint(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    token: &CapabilityToken,
    text: &str,
    options: FnxReadOptions,
) -> (String, usize, usize) {
    let probe = Arc::new(SimulationCheckpointProbe::new(None));
    let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
    let reads = Cell::new(0);
    let result = call(db, &observed, token, text, options, || {
        reads.set(reads.get() + 1);
        100
    });
    (format!("{result:?}"), probe.calls(), reads.get())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Live {
    Poll,
    Spend(Step),
}

/// The structural law under every reuse: `build` reaches the live execution
/// only through `poll` and `spend`, and its tape replays exactly that call
/// sequence. Covers every cut, both scope classes and the selection,
/// self-loop, parallel-edge and weight variants, including failed builds.
#[test]
fn a_tape_replays_every_live_call_of_its_build_in_order() {
    let ((), report) = run_async_under_lab(0x0b7b_0001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let issuer = issuer();
        let mut scoped = open();
        scoped.relations = Scope::only([RelationId(1)]);
        scoped.labels = Scope::only([LabelId(1)]);
        let (open, scoped) = (
            issuer.issue_at(&open(), 100).unwrap(),
            issuer.issue_at(&scoped, 100).unwrap(),
        );
        let frontier = db.frontier().unwrap();
        let mut variants = Vec::new();
        for direction in [Directedness::Directed, Directedness::Undirected] {
            let base = options(direction);
            variants.push(base);
            let mut other = base;
            other.selection.vertex_label = Some(LabelId(2));
            variants.push(other);
            other = base;
            other.selection.relation = Some(RelationId(2));
            variants.push(other);
            other = base;
            other.projection.self_loops = SelfLoopPolicy::Drop;
            variants.push(other);
            other.projection.self_loops = SelfLoopPolicy::Reject;
            variants.push(other);
            other = base;
            other.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
            variants.push(other);
            other = base;
            other.selection.weight = FnxWeightSpec::Property {
                key: W,
                missing: MissingWeightPolicy::Reject,
            };
            variants.push(other);
        }
        let (mut built, mut refused) = (0, 0);
        for token in [&open, &scoped] {
            let verified = issuer.verify_at(token, BRANCH, 100).unwrap();
            let scope = verified.predicates();
            for at in 0..=frontier.0 {
                for &variant in &variants {
                    let binding = SnapshotBinding {
                        root: db.snapshot.root.0,
                        as_of: CommitSeq(at),
                    };
                    let log = RefCell::new(Vec::new());
                    let poll = || {
                        log.borrow_mut().push(Live::Poll);
                        Ok(())
                    };
                    let spend = |step| {
                        log.borrow_mut().push(Live::Spend(step));
                        Ok(())
                    };
                    let tape = RefCell::new(Tape::default());
                    let result = build(
                        &db.snapshot,
                        binding,
                        variant,
                        scope,
                        Some(&tape),
                        poll,
                        spend,
                    );
                    let recorded = log.take();
                    if result.is_err() {
                        refused += 1;
                        continue;
                    }
                    built += 1;
                    let tape = tape.into_inner().finish();
                    assert!(tape.runs.len() <= recorded.len());
                    tape.replay(poll, spend).unwrap();
                    assert_eq!(log.take(), recorded, "cut {at} {variant:?}");
                }
            }
        }
        assert!(
            built > 50 && refused > 5,
            "{built} built, {refused} refused"
        );
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Over a single-version graph the tape is a handful of runs however many
/// records it admits: the retained cost is the projection, not the tape.
#[test]
fn a_single_version_tape_is_constant_size() {
    let ((), report) = run_async_under_lab(0x0b7b_0002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.commit();
        let mut db = Database::open_memory(&cx, DatabaseKeys::new([0x0c; 32], NS, [0x0d; 32]))
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 0..400u128 {
            batch.create_vertex(VId(id), vec![LabelId(1)], vec![]);
        }
        for id in 0..400u128 {
            batch.add_edge(EId(id), VId(id), VId((id * 7 + 1) % 400), weight(1));
        }
        db.write(&cx, batch).await.unwrap();
        let issuer = issuer();
        let token = issuer.issue_at(&open(), 100).unwrap();
        let verified = issuer.verify_at(&token, BRANCH, 100).unwrap();
        let tape = RefCell::new(Tape::default());
        let mut options = options(Directedness::Directed);
        options.source_limits.max_work_units = 10_000_000;
        build(
            &db.snapshot,
            SnapshotBinding {
                root: db.snapshot.root.0,
                as_of: db.frontier().unwrap(),
            },
            options,
            verified.predicates(),
            Some(&tape),
            || Ok(()),
            |_| Ok(()),
        )
        .unwrap();
        let tape = tape.into_inner().finish();
        assert!(tape.runs.len() <= 12, "{:?}", tape.runs);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A hit spends what the miss spent: equal rows, query-context checkpoints and
/// clock reads, and every checkpoint ordinal interrupts the hit exactly there.
#[test]
fn a_reused_projection_spends_exactly_what_its_build_spent() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x0b7b_0003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        for scoped in [false, true] {
            let db = database(&c.commit()).await;
            let mut grant = open();
            if scoped {
                grant.relations = Scope::only([RelationId(1)]);
            }
            let token = issuer().issue_at(&grant, 100).unwrap();
            for (text, direction) in CALLS {
                let options = options(direction);
                let before = builds();
                let cold = footprint(&db, &cx, &token, text, options);
                assert_eq!(builds(), before + 1, "{text}: the first call builds");
                assert!(cold.0.starts_with("Ok("), "{cold:?}");
                let bulk = BULK_RECORDS.with(Cell::get);
                let warm = footprint(&db, &cx, &token, text, options);
                assert_eq!(builds(), before + 1, "{text}: the second call reuses");
                assert_eq!(warm, cold, "{text}");
                // The hit replays its admitted records in bulk where the meters
                // cannot refuse, and still spends exactly what the build spent.
                assert!(
                    BULK_RECORDS.with(Cell::get) > bulk,
                    "{text}: a wide-allowance hit replays its records in bulk"
                );
                for stop in 1..=cold.1 {
                    let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                    let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                    assert!(
                        matches!(
                            call(&db, &controlled, &token, text, options, || 100),
                            Err(FnxReadError::Cancelled(QueryError::Pattern(
                                fgdb_gql::GqlQueryError::Interrupted(_)
                            )))
                        ),
                        "{text} stop {stop}"
                    );
                    assert_eq!(probe.calls(), stop, "{text} stop {stop}");
                }
                assert_eq!(builds(), before + 1, "{text}: interrupted hits still reuse");
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// The signed MaxWork and MaxNodes a holder may attenuate refuse a hit at the
/// same threshold, with the same refusal, as a fresh build on a database that
/// has never reused anything.
#[test]
fn signed_refusal_points_of_a_hit_match_a_fresh_build() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x0b7b_0004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let warm = database(&c.commit()).await;
        let token = issuer().issue_at(&open(), 100).unwrap();
        let limits: [fn(u64) -> Restriction; 2] = [Restriction::MaxWork, Restriction::MaxNodes];
        for (text, direction) in CALLS {
            let options = options(direction);
            call(&warm, &cx, &token, text, options, || 100).unwrap();
            for limit in limits {
                let attenuated = |value| token.attenuate(limit(value)).unwrap();
                let before = builds();
                // Smallest admitting limit on the warm (reusing) database.
                let (mut low, mut high) = (0, 10_000_000);
                while low < high {
                    let middle = low + (high - low) / 2;
                    if call(&warm, &cx, &attenuated(middle), text, options, || 100).is_ok() {
                        high = middle;
                    } else {
                        low = middle + 1;
                    }
                }
                assert_eq!(builds(), before, "{text}: every probe reused");
                assert!(low > 0);
                let refusal =
                    |db: &Database<MemVfs>| footprint(db, &cx, &attenuated(low - 1), text, options);
                let warm_refusal = refusal(&warm);
                assert!(warm_refusal.0.starts_with("Err("), "{warm_refusal:?}");
                // Fresh databases have nothing to reuse. A refusal after the
                // build (in the kernel or at delivery) may cache it, so the
                // admitted call gets its own fresh database too.
                let cold = database(&c.commit()).await;
                let before = builds();
                assert_eq!(refusal(&cold), warm_refusal, "{text} at {}", low - 1);
                assert!(builds() <= before + 1);
                let cold = database(&c.commit()).await;
                let before = builds();
                let admitted = footprint(&cold, &cx, &attenuated(low), text, options);
                assert_eq!(builds(), before + 1, "{text}: the cold call built");
                assert_eq!(
                    footprint(&warm, &cx, &attenuated(low), text, options),
                    admitted
                );
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Kernel units charged in runs (fgdb-62mzm) admit and refuse exactly where
/// charging every unit alone does: the same smallest admitting MaxWork, the
/// same refusal one unit below it and the same admitted rows, with no more
/// query-context polls.
#[test]
fn kernel_runs_admit_and_refuse_at_the_per_unit_threshold() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x0b7b_0009, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let token = issuer().issue_at(&open(), 100).unwrap();
        for (text, direction, self_loops) in CALLS
            .into_iter()
            .map(|(text, direction)| (text, direction, SelfLoopPolicy::Keep))
            .chain([
                (
                    "CALL fnx.weakly_connected_components() YIELD vertex,component",
                    Directedness::Directed,
                    SelfLoopPolicy::Keep,
                ),
                (
                    "CALL fnx.degree_centrality() YIELD vertex,score",
                    Directedness::Undirected,
                    SelfLoopPolicy::Drop,
                ),
                (
                    "CALL fnx.core_number() YIELD vertex,core",
                    Directedness::Undirected,
                    SelfLoopPolicy::Drop,
                ),
            ])
        {
            let mut options = options(direction);
            // The common database includes a kept self-loop. These simple
            // undirected procedures must explicitly request its projection drop.
            options.projection.self_loops = self_loops;
            let threshold = |runs: bool| {
                super::super::WORK_RUNS.with(|cell| cell.set(runs));
                let attenuated = |value| token.attenuate(Restriction::MaxWork(value)).unwrap();
                let (mut low, mut high) = (0, 10_000_000);
                while low < high {
                    let middle = low + (high - low) / 2;
                    if call(&db, &cx, &attenuated(middle), text, options, || 100).is_ok() {
                        high = middle;
                    } else {
                        low = middle + 1;
                    }
                }
                assert!(low > 0, "{text}");
                let refused = footprint(&db, &cx, &attenuated(low - 1), text, options);
                let admitted = footprint(&db, &cx, &attenuated(low), text, options);
                super::super::WORK_RUNS.with(|cell| cell.set(true));
                (low, refused, admitted)
            };
            let (unit_low, unit_refused, unit_admitted) = threshold(false);
            let (run_low, run_refused, run_admitted) = threshold(true);
            assert_eq!(run_low, unit_low, "{text}: admitting MaxWork");
            assert!(run_refused.0.starts_with("Err("), "{run_refused:?}");
            assert_eq!(run_refused.0, unit_refused.0, "{text}: refusal");
            assert_eq!(run_admitted.0, unit_admitted.0, "{text}: rows");
            assert!(run_admitted.1 <= unit_admitted.1, "{text}: polls");
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A signed refusal must not overtake an earlier native refusal in a kernel
/// run. Compare with explicit signed-then-native unit charges, including both
/// counters, partial deferred runs and the live control transcript. When the
/// native allowance admits the run, it still receives one bulk observation.
#[test]
fn kernel_bulk_work_preserves_crossed_allowance_refusals_and_control_prefixes() {
    #[derive(Clone, Copy, Debug)]
    struct Case {
        prefix: u64,
        signed: u64,
        native: u64,
        units: usize,
        stop: Option<usize>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Footprint {
        outcome: String,
        signed: fgdb_warden::Usage,
        native: AdmissionUsage,
        deferred: u64,
        run_left: u64,
        polls: usize,
        clock_reads: usize,
    }

    /// An interruption's Debug text carries the runtime's process-wide
    /// correlation counter, which no two runs share; every other field of
    /// the outcome is compared as printed.
    fn outcome(debug: String) -> String {
        const KEY: &str = "correlation_id: Some(";
        let mut out = String::with_capacity(debug.len());
        let mut rest = debug.as_str();
        while let Some(at) = rest.find(KEY) {
            out.push_str(&rest[..at]);
            out.push_str("correlation_id: _");
            let tail = &rest[at + KEY.len()..];
            rest = tail.find(')').map_or("", |end| &tail[end + 1..]);
        }
        out.push_str(rest);
        out
    }

    struct ObservedUsage<'a> {
        inner: UsageObserver<'a>,
        largest_run: usize,
    }
    impl Observe for ObservedUsage<'_> {
        fn event(&mut self, event: SourceEvent) -> Result<(), QueryError> {
            self.inner.event(event)
        }
        fn work(&mut self, n: usize) -> Result<(), QueryError> {
            self.largest_run = self.largest_run.max(n);
            self.inner.work(n)
        }
        fn absorbs(&self, records: u64, work: u64, scratch: u64) -> bool {
            self.inner.absorbs(records, work, scratch)
        }
    }

    let ((), report) = run_async_under_lab(0x0b7b_000a, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let authority = issuer();
        let token = authority.issue_at(&open(), 100).unwrap();
        let run = |case: Case, bulk: bool| {
            let token = token
                .attenuate(Restriction::MaxWork(case.prefix + case.signed))
                .unwrap();
            let verified = authority.verify_at(&token, BRANCH, 100).unwrap();
            let permit = verified.begin_read_at(BRANCH, 100).unwrap();
            let reads = Cell::new(0usize);
            let mut execution = Execution::new(&cx, permit, || {
                reads.set(reads.get() + 1);
                100
            });
            execution.in_runs = true;
            let policy = GqlQueryPolicy::new(1_000, 1_000, case.prefix + case.native, 1_000);
            let mut usage = AdmissionUsage::default();
            let mut observer = ObservedUsage {
                inner: UsageObserver {
                    usage: &mut usage,
                    policy,
                },
                largest_run: 0,
            };
            // Warm an arbitrary partial run before attaching the controlled
            // probe. Both executions begin the requested run in the same state.
            for _ in 0..case.prefix {
                execution.checkpoint().unwrap();
                observer.event(SourceEvent::Work).unwrap();
            }
            let probe = Arc::new(SimulationCheckpointProbe::new(case.stop));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            execution.cx = &controlled;
            reads.set(0);
            let mut result = if bulk {
                KernelMeter {
                    execution: &mut execution,
                    observe: &mut observer,
                }
                .units(case.units)
            } else {
                (0..case.units).try_for_each(|_| {
                    execution.checkpoint()?;
                    observer.event(SourceEvent::Work)
                })
            };
            if result.is_ok() {
                result = execution.settle();
            }
            (
                Footprint {
                    outcome: outcome(format!("{result:?}")),
                    signed: execution.permit.usage(),
                    native: *observer.inner.usage,
                    deferred: execution.deferred,
                    run_left: execution.run_left,
                    polls: probe.calls(),
                    clock_reads: reads.get(),
                },
                observer.largest_run,
            )
        };

        for prefix in [0, 3, 63, 65] {
            for units in [0, 1, 4, 64] {
                for signed in [0, 1, 3, 4, 63, 64, 65, 256] {
                    for native in [0, 1, 3, 4, 63, 64, 65, 256] {
                        let case = Case {
                            prefix,
                            signed,
                            native,
                            units,
                            stop: None,
                        };
                        let (actual, largest_run) = run(case, true);
                        let (expected, _) = run(case, false);
                        assert_eq!(actual, expected, "{case:?}");
                        if units as u64 <= signed.min(native) {
                            assert_eq!(actual.outcome, "Ok(())", "{case:?}");
                            if units > 1 {
                                assert_eq!(largest_run, units, "{case:?}: bulk observation");
                            }
                        } else if signed <= native {
                            assert_eq!(
                                actual.outcome, "Err(Authorization(LimitExceeded(Work)))",
                                "{case:?}: signed refusal comes first"
                            );
                        } else {
                            assert!(
                                actual.outcome.starts_with("Err(Pattern(Evaluator("),
                                "{case:?}: native refusal comes first: {actual:?}"
                            );
                        }
                    }
                }
            }
        }

        // Cancellation at a settlement inside the requested run leaves only
        // its completed prefix observed; cancellation on final settlement
        // leaves the whole run observed. Both must match the scalar path.
        for prefix in [1, 3, 63, 65] {
            for stop in [1, 2] {
                let case = Case {
                    prefix,
                    signed: 256,
                    native: 256,
                    units: 64,
                    stop: Some(stop),
                };
                let (actual, _) = run(case, true);
                let (expected, _) = run(case, false);
                assert_eq!(actual, expected, "{case:?}");
                assert!(actual.outcome.starts_with("Err("), "{case:?}");
                assert_eq!(actual.polls, stop, "{case:?}");
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Arithmetic checks use the same live context and authority as metered work,
/// while preserving signed/native usage and any partially prepaid work run.
#[test]
fn kernel_live_polls_preserve_charges_and_observe_cancellation_expiry_and_retirement() {
    let ((), report) = run_async_under_lab(0x0b7b_000b, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c
            .query()
            .with_host_clock_interval(std::num::NonZeroU32::new(128).unwrap());
        for cause in ["cancellation", "expiry", "retirement"] {
            let authority = issuer();
            let token = authority.issue_at(&open(), 100).unwrap();
            let verified = authority.verify_at(&token, BRANCH, 100).unwrap();
            let permit = verified.begin_read_at(BRANCH, 100).unwrap();
            let now = Cell::new(100);
            let probe = Arc::new(SimulationCheckpointProbe::new(
                (cause == "cancellation").then_some(2),
            ));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let mut execution = Execution::new(&controlled, permit, || now.get());
            execution.in_runs = true;
            // Admission of this deferred prefix does not poll. The live
            // arithmetic poll must neither settle it nor add a unit to it.
            execution.work_units(3).unwrap();
            let policy = GqlQueryPolicy::new(1_000, 1_000, 1_000, 1_000);
            let mut usage = AdmissionUsage::default();
            let mut observer = UsageObserver {
                usage: &mut usage,
                policy,
            };
            observer.work(3).unwrap();
            let before = (
                execution.permit.usage(),
                *observer.usage,
                execution.deferred,
                execution.run_left,
            );
            KernelMeter {
                execution: &mut execution,
                observe: &mut observer,
            }
            .poll()
            .unwrap();
            match cause {
                "expiry" => now.set(10_001),
                "retirement" => {
                    assert!(authority.retire());
                }
                _ => {}
            }
            let result = KernelMeter {
                execution: &mut execution,
                observe: &mut observer,
            }
            .poll();
            assert!(
                matches!(
                    (cause, &result),
                    (
                        "cancellation",
                        Err(QueryError::Pattern(fgdb_gql::GqlQueryError::Interrupted(_)))
                    ) | (
                        "expiry",
                        Err(QueryError::Authorization(fgdb_warden::Error::Expired))
                    ) | (
                        "retirement",
                        Err(QueryError::Authorization(
                            fgdb_warden::Error::AuthorityRetired
                        ))
                    )
                ),
                "{cause} must retain its own refusal: {result:?}",
            );
            assert_eq!(
                (
                    execution.permit.usage(),
                    *observer.usage,
                    execution.deferred,
                    execution.run_left,
                ),
                before,
                "{cause}: live checks spend no work and preserve the prefix",
            );
            assert_eq!(probe.calls(), 2);
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Keep the remote cancellation-prefix law alongside the expiry/retirement
/// cases. Polls sample the live clock, but never settle or add prepaid work.
#[test]
fn kernel_live_polls_cancel_without_changing_any_work_accounting() {
    let ((), report) = run_async_under_lab(0x0b7b_000c, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let authority = issuer();
        let token = authority.issue_at(&open(), 100).unwrap();
        for stop in [None, Some(1), Some(3)] {
            let verified = authority.verify_at(&token, BRANCH, 100).unwrap();
            let permit = verified.begin_read_at(BRANCH, 100).unwrap();
            let reads = Cell::new(0usize);
            let mut execution = Execution::new(&cx, permit, || {
                reads.set(reads.get() + 1);
                100
            });
            execution.in_runs = true;
            let mut usage = AdmissionUsage::default();
            let mut observer = UsageObserver {
                usage: &mut usage,
                policy: GqlQueryPolicy::new(1_000, 1_000, 1_000, 1_000),
            };
            KernelMeter {
                execution: &mut execution,
                observe: &mut observer,
            }
            .units(17)
            .unwrap();
            let before = (
                execution.permit.usage(),
                *observer.usage,
                execution.deferred,
                execution.run_left,
            );
            let clock_before = reads.get();
            assert!(before.2 > 0, "exercise a real deferred charge");
            let probe = Arc::new(SimulationCheckpointProbe::new(stop));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            execution.cx = &controlled;
            let mut meter = KernelMeter {
                execution: &mut execution,
                observe: &mut observer,
            };
            let result = (0..4).try_for_each(|_| meter.poll());
            if let Some(stop) = stop {
                assert!(matches!(
                    result,
                    Err(QueryError::Pattern(fgdb_gql::GqlQueryError::Interrupted(_)))
                ));
                assert_eq!(probe.calls(), stop);
            } else {
                assert!(result.is_ok());
                assert_eq!(probe.calls(), 4);
            }
            assert_eq!(
                (
                    execution.permit.usage(),
                    *observer.usage,
                    execution.deferred,
                    execution.run_left,
                ),
                before,
                "stop={stop:?}: a live poll must not mutate accounting"
            );
            assert_eq!(
                reads.get() - clock_before,
                stop.map_or(4, |stop| stop - 1),
                "each uncancelled arithmetic poll samples fresh authorization time"
            );
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Each label, relation or property scope reuses its own exact projection,
/// never the unrestricted projection cached before it. A fresh database is
/// the independent source/build oracle; a scoped entry cannot serve an open
/// caller even when both requests have the same snapshot root bytes.
#[test]
fn scoped_capabilities_reuse_only_their_exact_compiled_projection() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x0b7b_0005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let open = issuer().issue_at(&open(), 100).unwrap();
        let calls = [
            CALLS[1],
            (
                "CALL fnx.single_source_dijkstra_path_length(1,NULL,true) YIELD vertex,distance",
                Directedness::Directed,
            ),
        ];
        for restriction in [
            Restriction::Labels(Scope::only([LabelId(2)])),
            Restriction::Relations(Scope::only([RelationId(2)])),
            Restriction::Properties(Scope::only([PropertyKeyId(5)])),
            Restriction::DenyProperties([W].into()),
        ] {
            // At most eight live entries: every scope/call pair has room,
            // and dropping these owners releases the previous pair's slots.
            let db = database(&c.commit()).await;
            let pristine = database(&c.commit()).await;
            let token = open.attenuate(restriction.clone()).unwrap();
            let mut masked = false;
            for (text, direction) in calls {
                let options = options(direction);
                let unrestricted = footprint(&db, &cx, &open, text, options);
                let before = builds();
                let first = footprint(&db, &cx, &token, text, options);
                assert_eq!(builds(), before + 1, "a distinct mask must build");
                assert_eq!(
                    footprint(&pristine, &cx, &token, text, options),
                    first,
                    "{restriction:?} saw another scope's projection: {text}"
                );
                let second = footprint(&db, &cx, &token, text, options);
                assert_eq!(first, second);
                assert_eq!(builds(), before + 2, "{restriction:?} must reuse: {text}");
                masked |= unrestricted.0 != first.0;
                let before = builds();
                assert_eq!(
                    footprint(&pristine, &cx, &open, text, options),
                    unrestricted
                );
                assert_eq!(builds(), before + 1, "a scoped entry served an open caller");
            }
            assert!(masked, "{restriction:?} must change some rows");
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn changed_scoped_permissions_build_their_own_mask_before_reuse() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x0b7b_000d, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let authority = issuer();
        let mut grant = open();
        grant.labels = Scope::only([LabelId(1)]);
        let original = authority.issue_at(&grant, 100).unwrap();
        let text = "CALL fnx.single_source_dijkstra_path_length(1,NULL,true) YIELD vertex,distance";
        let options = options(Directedness::Directed);
        for restriction in [
            Restriction::Labels(Scope::only([LabelId(2)])),
            Restriction::Relations(Scope::only([RelationId(2)])),
            Restriction::DenyProperties([W].into()),
        ] {
            let db = database(&c.commit()).await;
            let cold = database(&c.commit()).await;
            let before = footprint(&db, &cx, &original, text, options);
            let changed = original.attenuate(restriction.clone()).unwrap();
            let builds_before = builds();
            let first = footprint(&db, &cx, &changed, text, options);
            assert_eq!(builds(), builds_before + 1, "{restriction:?}");
            let expected = footprint(&cold, &cx, &changed, text, options);
            assert_eq!(first, expected, "{restriction:?}");
            let built = builds();
            assert_eq!(footprint(&db, &cx, &changed, text, options), expected);
            assert_eq!(
                builds(),
                built,
                "the changed scope must reuse its own entry"
            );
            assert_eq!(footprint(&db, &cx, &original, text, options), before);
            assert_eq!(
                builds(),
                built,
                "attenuation must not replace the earlier key"
            );
            assert_ne!(first.0, before.0, "the fixture must expose {restriction:?}");
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scoped_hits_preserve_fresh_expiry_retirement_and_rights_checks() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x0b7b_000e, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let mut grant = open();
        grant.labels = Scope::only([LabelId(1)]);
        let authority = issuer();
        let token = authority.issue_at(&grant, 100).unwrap();
        let (text, direction) = CALLS[1];
        let options = options(direction);
        call(&db, &cx, &token, text, options, || 100).unwrap();
        let before = builds();
        let (_, _, boundaries) = footprint(&db, &cx, &token, text, options);
        assert_eq!(builds(), before, "the control must be a scoped cache hit");
        assert!(boundaries > 2);
        for retirement in [false, true] {
            for boundary in 1..=boundaries {
                let authority = issuer();
                let token = authority.issue_at(&grant, 100).unwrap();
                let calls = Cell::new(0);
                let result = db.call_fnx_authorized(
                    &cx,
                    &authority,
                    &token,
                    BRANCH,
                    text,
                    &FnxParameters::new(),
                    options,
                    || {
                        calls.set(calls.get() + 1);
                        if calls.get() >= boundary {
                            if retirement {
                                authority.retire();
                            } else {
                                return grant.expires_at_ms;
                            }
                        }
                        100
                    },
                );
                let expected = if retirement {
                    fgdb_warden::Error::AuthorityRetired
                } else {
                    fgdb_warden::Error::Expired
                };
                assert!(
                    matches!(result, Err(Error::Cancelled(QueryError::Authorization(error))) if error == expected),
                    "boundary {boundary}, retirement {retirement}: {result:?}"
                );
                assert_eq!(builds(), before, "a stopped hit must not rebuild");
            }
        }
        let denied = token
            .attenuate(Restriction::Rights(fgdb_warden::Rights::None))
            .unwrap();
        assert!(matches!(
            call(&db, &cx, &denied, text, options, || 100),
            Err(Error::Cancelled(QueryError::Authorization(
                fgdb_warden::Error::PermissionDenied
            )))
        ));
        assert_eq!(builds(), before);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn change() -> WriteBatch {
    // Joins the {1,2,6,7} and {3,4,5,8} components and cuts 7 -> 1.
    let mut change = WriteBatch::new(RelationId(1));
    change.add_edge(EId(1000), VId(2), VId(3), weight(1));
    change.delete_edge(EId(8));
    change
}

/// A commit after a reuse yields the new generation's rows, equal to a
/// database that never cached; the old cut still yields the old rows. A view
/// pinning the old generation keeps it alive, so the commit copies the
/// snapshot and the cached entry stays with the old allocation.
#[test]
fn a_commit_never_serves_the_previous_generation() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x0b7b_0006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let token = issuer().issue_at(&open(), 100).unwrap();
        for (text, direction) in CALLS {
            let options = options(direction);
            for pin in [false, true] {
                let mut db = database(&c.commit()).await;
                let old_cut = db.frontier().unwrap();
                let old = call(&db, &cx, &token, text, options, || 100).unwrap();
                let before = builds();
                assert_eq!(call(&db, &cx, &token, text, options, || 100).unwrap(), old);
                assert_eq!(builds(), before, "{text}: reused before the commit");
                let pinned = pin.then(|| db.read_session().unwrap());
                db.write(&c.commit(), change()).await.unwrap();
                let new = call(&db, &cx, &token, text, options, || 100).unwrap();
                assert_eq!(builds(), before + 1, "{text}: the new generation builds");
                assert_ne!(new, old, "{text}: the change must move the rows");
                let mut oracle = database(&c.commit()).await;
                oracle.write(&c.commit(), change()).await.unwrap();
                assert_eq!(
                    call(&oracle, &cx, &token, text, options, || 100).unwrap(),
                    new
                );
                let mut historical = options;
                historical.as_of = Some(old_cut);
                assert_eq!(
                    call(&db, &cx, &token, text, historical, || 100).unwrap(),
                    old
                );
                drop(pinned);
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// The gauntlet's path: `CALL fnx.*` composed inside an authorized native
/// read reuses across statements and returns the privileged read's rows.
#[test]
fn a_composed_call_reuses_across_authorized_native_reads() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x0b7b_0007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = database(&c.commit()).await;
        // A composed CALL projects with the strict multigraph policy.
        let mut simple = WriteBatch::new(RelationId(1));
        simple.delete_edge(EId(6));
        db.write(&c.commit(), simple).await.unwrap();
        let token = issuer().issue_at(&open(), 100).unwrap();
        let params = GqlParameters::new();
        let policy = GqlQueryPolicy::new(100_000, 1_000, 1_000_000, 1_000_000);
        let symbols = |_: GraphSymbolKind, _: &str| None;
        // Both procedures request the same projection, so the first read
        // builds it and every later read of either reuses it.
        let before = builds();
        for (at, text) in [
            "CALL fnx.pagerank() YIELD vertex, score RETURN vertex, score \
             ORDER BY score DESC, vertex LIMIT 3",
            "CALL fnx.weakly_connected_components() YIELD vertex, component \
             RETURN count(DISTINCT component) AS components",
        ]
        .into_iter()
        .enumerate()
        {
            let read = || {
                db.query_authorized(
                    &cx,
                    &issuer(),
                    &token,
                    BRANCH,
                    text,
                    &params,
                    symbols,
                    policy,
                    || 100,
                )
                .unwrap()
            };
            let first = read();
            assert_eq!(builds(), before + 1, "{text} (statement {at})");
            assert_eq!(read(), first, "{text}");
            assert_eq!(builds(), before + 1, "{text}: the second read reuses");
            assert_eq!(
                first,
                db.query(&cx, text, &params, symbols, policy).unwrap(),
                "{text}"
            );
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Under the composed path's caller meter, a hit meets every allowance where
/// its build did: at each dimension's least admitting allowance and one below
/// it, a read answers or refuses exactly as a fresh build does. A wide
/// allowance replays the hit in bulk; a tight one falls back to replaying it
/// item by item around the refusal. The record and scratch allowances are
/// also the call's source limits, so they are part of its reuse key: only a
/// read under the allowance a successful one had reuses its graph. The
/// evaluator work allowance is not, so a hit itself meets that refusal.
#[test]
fn a_reused_projection_meets_the_caller_meter_where_its_build_did() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x0b7b_0008, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let composed = async || {
            let mut db = database(&c.commit()).await;
            let mut simple = WriteBatch::new(RelationId(1));
            simple.delete_edge(EId(6));
            db.write(&c.commit(), simple).await.unwrap();
            db
        };
        for scoped in [false, true] {
            let warm = composed().await;
            let mut grant = open();
            if scoped {
                grant.labels = Scope::only([LabelId(1)]);
                grant.properties = Scope::only([W]);
            }
            let token = issuer().issue_at(&grant, 100).unwrap();
            let params = GqlParameters::new();
            let symbols = |_: GraphSymbolKind, _: &str| None;
            let read = |db: &Database<MemVfs>, text: &str, policy: GqlQueryPolicy| {
                format!(
                    "{:?}",
                    db.query_authorized(
                        &cx,
                        &issuer(),
                        &token,
                        BRANCH,
                        text,
                        &params,
                        symbols,
                        policy,
                        || 100
                    )
                )
            };
            let wide = [100_000, 1_000, 1_000_000, 1_000_000];
            let policy =
                |limits: [u64; 4]| GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]);
            for text in [
                "CALL fnx.pagerank() YIELD vertex, score RETURN vertex, score \
             ORDER BY score DESC, vertex LIMIT 3",
                "CALL fnx.weakly_connected_components() YIELD vertex, component \
             RETURN count(DISTINCT component) AS components",
            ] {
                assert!(read(&warm, text, policy(wide)).starts_with("Ok("), "{text}");
                let bulk = BULK_RECORDS.with(Cell::get);
                assert!(read(&warm, text, policy(wide)).starts_with("Ok("), "{text}");
                assert!(
                    BULK_RECORDS.with(Cell::get) > bulk,
                    "{text}: a wide-allowance hit replays its records in bulk"
                );
                // Snapshot records, evaluator work and evaluator scratch.
                for dimension in [0, 2, 3] {
                    let at = |n: u64| {
                        let mut limits = wide;
                        limits[dimension] = n;
                        policy(limits)
                    };
                    let (mut low, mut high) = (0, wide[dimension]);
                    while low < high {
                        let middle = low + (high - low) / 2;
                        // A fresh database each time: a successful read under a
                        // new record or scratch allowance keeps a graph, and the
                        // sidecar's eight entries never displace one.
                        if read(&composed().await, text, at(middle)).starts_with("Ok(") {
                            high = middle;
                        } else {
                            low = middle + 1;
                        }
                    }
                    assert!(low > 0, "{text}: dimension {dimension}");
                    for n in [low - 1, low] {
                        let cold = read(&composed().await, text, at(n));
                        let built = builds();
                        let first = read(&warm, text, at(n));
                        if dimension == 2 {
                            assert_eq!(
                                builds(),
                                built,
                                "{text}: work at {n} reuses the wide graph"
                            );
                        }
                        let built = builds();
                        let again = read(&warm, text, at(n));
                        if again.starts_with("Ok(") {
                            assert_eq!(
                                builds(),
                                built,
                                "{text}: dimension {dimension} at {n} reuses"
                            );
                        }
                        assert_eq!(first, cold, "{text}: dimension {dimension} at {n}");
                        assert_eq!(again, cold, "{text}: dimension {dimension} at {n}");
                    }
                }
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
