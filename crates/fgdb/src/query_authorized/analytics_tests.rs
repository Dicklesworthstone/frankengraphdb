//! fgdb-0tbrz: a reused authorized projection is the build it replaces. The
//! build's live calls replay in order, so rows, every signed refusal point and
//! every cancellation ordinal agree with a fresh build. Scoped capabilities
//! never reuse, and a commit never serves the previous generation's graph.

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
/// lawfully builds, so the tests that count builds take turns.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let db = database(&c.commit()).await;
        let token = issuer().issue_at(&open(), 100).unwrap();
        for (text, direction) in CALLS {
            let options = options(direction);
            let before = builds();
            let cold = footprint(&db, &cx, &token, text, options);
            assert_eq!(builds(), before + 1, "{text}: the first call builds");
            assert!(cold.0.starts_with("Ok("), "{cold:?}");
            let warm = footprint(&db, &cx, &token, text, options);
            assert_eq!(builds(), before + 1, "{text}: the second call reuses");
            assert_eq!(warm, cold, "{text}");
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
        for (text, direction) in CALLS {
            let options = options(direction);
            let threshold = |runs: bool| {
                super::super::KERNEL_RUNS.with(|cell| cell.set(runs));
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
                super::super::KERNEL_RUNS.with(|cell| cell.set(true));
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

/// A capability with any label, relation or property scope builds its own
/// projection every time: it never sees the unrestricted projection cached
/// before it, and leaves nothing for an unrestricted caller. A database that
/// never cached anything is the oracle for its rows.
#[test]
fn scoped_capabilities_never_reuse_or_populate() {
    let _serial = serial();
    let ((), report) = run_async_under_lab(0x0b7b_0005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let pristine = database(&c.commit()).await;
        let open = issuer().issue_at(&open(), 100).unwrap();
        let calls = [
            CALLS[1],
            (
                "CALL fnx.single_source_dijkstra_path_length(1,NULL,true) YIELD vertex,distance",
                Directedness::Directed,
            ),
        ];
        let unrestricted: Vec<_> = calls
            .iter()
            .map(|&(text, direction)| {
                format!(
                    "{:?}",
                    call(&db, &cx, &open, text, options(direction), || 100)
                )
            })
            .collect();
        for restriction in [
            Restriction::Labels(Scope::only([LabelId(2)])),
            Restriction::Relations(Scope::only([RelationId(2)])),
            Restriction::Properties(Scope::only([PropertyKeyId(5)])),
            Restriction::DenyProperties([W].into()),
        ] {
            let token = open.attenuate(restriction.clone()).unwrap();
            let mut masked = false;
            for (&(text, direction), unrestricted) in calls.iter().zip(&unrestricted) {
                let options = options(direction);
                let before = builds();
                let first = format!("{:?}", call(&db, &cx, &token, text, options, || 100));
                assert_eq!(
                    format!("{:?}", call(&pristine, &cx, &token, text, options, || 100)),
                    first,
                    "{restriction:?} saw another scope's projection: {text}"
                );
                let second = format!("{:?}", call(&db, &cx, &token, text, options, || 100));
                assert_eq!(first, second);
                assert_eq!(builds(), before + 3, "{restriction:?} reused: {text}");
                masked |= *unrestricted != first;
            }
            assert!(masked, "{restriction:?} must change some rows");
        }
        let (text, direction) = calls[1];
        let before = builds();
        call(&pristine, &cx, &open, text, options(direction), || 100).unwrap();
        assert_eq!(builds(), before + 1, "a scoped build populated the cache");
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
