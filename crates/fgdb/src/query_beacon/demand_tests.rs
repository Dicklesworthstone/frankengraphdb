//! Real admitted snapshots exercise the new physical source, not a second BFS.
//! Expected endpoints/ranks are literal; the old indexed profile is an extra
//! differential and resource-limit control, not the only oracle.

use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_beacon::expansion::ExpansionLimits;
use fgdb_beacon::{ExactRrfProfile, WorkBudget};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x56; 32]);
const R: RelationId = RelationId(1);
const HIGH: VId = VId(u128::MAX);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x55; 32], NS, [0x57; 32])
}

fn options() -> Options {
    let mut options = Options::text(PropertyKeyId(9));
    options.vertex_label = Some(LabelId(1));
    // A topology-only source cannot hide a synthetic text/index requirement.
    options.index.text = None;
    options.index.vector = None;
    options
}

fn expansion(seeds: &[VId]) -> ExpansionSpec<'_, RelationId> {
    ExpansionSpec {
        seeds,
        relation: Some(R),
        direction: ExpansionDirection::Outgoing,
        max_hops: 2,
        include_seeds: false,
        limits: ExpansionLimits::default(),
    }
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, disconnected: usize, hidden: bool) {
    let mut batch = WriteBatch::new(R);
    for id in [VId(0), VId(2), VId(9), HIGH] {
        batch.create_vertex(id, vec![LabelId(1)], vec![(PropertyKeyId(9), CanonicalScalar::Bool(true))]);
    }
    for (eid, src, dst) in [
        (0, VId(0), VId(2)),
        (1, VId(0), VId(2)),
        (2, VId(2), HIGH),
        (3, VId(2), VId(2)),
        (u128::MAX, HIGH, VId(0)),
    ] {
        batch.add_edge(EId(eid), src, dst, vec![]);
    }
    for n in 0..disconnected {
        let id = 1_000 + n as u128;
        batch.create_vertex(VId(id), vec![LabelId(1)], vec![]);
        batch.add_edge(EId(id), VId(id), VId(id), vec![]);
    }
    if hidden {
        batch.create_vertex(VId(90), vec![LabelId(99)], vec![]);
        batch.add_edge(EId(90), VId(0), VId(90), vec![]);
        batch.add_edge(EId(91), VId(90), VId(9), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut shortcut = WriteBatch::new(RelationId(99));
        shortcut.add_edge(EId(92), VId(0), HIGH, vec![]);
        db.write(cx, shortcut).await.unwrap();
    }
}

fn endpoints(rows: &[GraphHybridHit]) -> Vec<(VId, u32)> {
    rows.iter().map(|row| (row.id, row.graph_hops.unwrap())).collect()
}

#[test]
fn reached_membership_replaces_the_full_directory_without_changing_ranks() {
    let ((), report) = run_async_under_lab(0xbeac_5101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit(), 1_024, false).await;
        let q = GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap();
        let spec = expansion(&[VId(0)]);
        let expected = db.beacon_search_graph_indexed_filtered(
            &c.query(), &options(), q, spec, |id| id == HIGH,
        ).unwrap();
        let mut limited = options();
        limited.policy.max_staging_rows = 3;
        limited.index.max_documents = 3;
        let mut spec = spec;
        spec.limits.max_vertices = 3;
        spec.limits.max_input_edges = 4;
        let actual = db.beacon_search_graph_demand_filtered(
            &c.query(), &limited, q, spec, |id| id == HIGH,
        ).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(endpoints(&actual), [(HIGH, 2)]);
        assert_eq!(actual[0].graph_rank.unwrap().get(), 1);
        assert_eq!((actual[0].score.numerator(), actual[0].score.denominator()), (100, 61));
        assert_eq!(actual[0].vector_rank, None);
        assert_eq!(actual[0].text_rank, None);
        assert!(db.beacon_search_graph_indexed_filtered(
            &c.query(), &limited, q, spec, |id| id == HIGH,
        ).is_err(), "the complete-directory source must not fit a three-vertex allowance");
        spec.limits.max_input_edges = 3;
        assert!(matches!(db.beacon_search_graph_demand_filtered(
            &c.query(), &limited, q, spec, |_| false,
        ), Err(Error::Index(BeaconError::ResourceLimit {
            resource: "demand graph incidence visits", limit: 3,
        }))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn directions_zero_hops_and_full_width_ids_have_literal_expected_answers() {
    let ((), report) = run_async_under_lab(0xbeac_5102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit(), 0, false).await;
        let q = GraphHybridQuery::graph_only(4, 4, 1, 60).unwrap();
        for (direction, expected) in [
            (ExpansionDirection::Outgoing, vec![(VId(2), 1), (HIGH, 2)]),
            (ExpansionDirection::Incoming, vec![(HIGH, 1), (VId(2), 2)]),
            (ExpansionDirection::Undirected, vec![(VId(2), 1), (HIGH, 1)]),
        ] {
            let mut spec = expansion(&[VId(0), VId(0), VId(999)]);
            spec.direction = direction;
            let rows = db.beacon_search_graph_demand_filtered(
                &c.query(), &options(), q, spec, |_| true,
            ).unwrap();
            assert_eq!(endpoints(&rows), expected);
            assert_eq!(rows, db.beacon_search_graph_indexed(
                &c.query(), &options(), q, spec,
            ).unwrap());
        }
        let mut spec = expansion(&[VId(9), HIGH, VId(9), VId(999)]);
        spec.max_hops = 0;
        spec.include_seeds = true;
        spec.limits.max_input_edges = 0;
        let work = RefCell::new(WorkBudget::new(100_000));
        let nodes = Cell::new(0);
        let (rows, scratch) = evaluate_filtered(
            &db.snapshot, db.frontier().unwrap(), &options(), q, spec,
            &work, Scan::Metered,
            |_| { nodes.set(nodes.get() + 1); Ok(true) }, |_| true, |_| true,
            || panic!("zero-hop retrieval admitted an incidence"), |_| true,
        ).unwrap();
        assert_eq!(endpoints(&rows), [(VId(9), 0), (HIGH, 0)]);
        assert_eq!(nodes.get(), 2, "duplicates and missing seeds cannot become extra nodes");
        assert_eq!(scratch, 2, "only the two admitted cache entries are retained");
        spec.seeds = &[];
        assert!(db.beacon_search_graph_demand_filtered(
            &c.query(), &options(), q, spec, |_| true,
        ).unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pinned_winners_survive_retirement_label_changes_compaction_reopen_and_writer_drop() {
    let ((), report) = run_async_under_lab(0xbeac_5103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&c.commit(), vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &c.commit(), 0, false).await;
        let at = db.frontier().unwrap();
        let pin = db.read_session().unwrap();
        let q = GraphHybridQuery::graph_only(1, 1, 1, 60).unwrap();
        let spec = expansion(&[VId(0)]);
        let before = pin.beacon_search_graph_demand_filtered(
            &c.query(), &options(), q, spec, |id| id == HIGH,
        ).unwrap();
        assert_eq!(endpoints(&before), [(HIGH, 2)]);
        let mut edit = WriteBatch::new(R);
        edit.delete_edge(EId(0)); // The parallel survivor still carries the path.
        db.write(&c.commit(), edit).await.unwrap();
        assert_eq!(db.beacon_search_graph_demand_filtered(
            &c.query(), &options(), q, spec, |id| id == HIGH,
        ).unwrap(), before);
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_label(VId(2), LabelId(1), false);
        db.write(&c.commit(), edit).await.unwrap();
        assert!(db.beacon_search_graph_demand_filtered(
            &c.query(), &options(), q, spec, |id| id == HIGH,
        ).unwrap().is_empty(), "excluded source labels remove transit, not merely results");
        let mut edit = WriteBatch::new(R);
        edit.delete_edge(EId(2));
        db.write(&c.commit(), edit).await.unwrap();
        db.compact(&c.commit()).await.unwrap();
        let mut historic = options();
        historic.as_of = Some(at);
        assert_eq!(db.beacon_search_graph_demand_filtered(
            &c.query(), &historic, q, spec, |id| id == HIGH,
        ).unwrap(), before);
        historic.as_of = Some(CommitSeq(at.0 + 1));
        assert!(matches!(pin.beacon_search_graph_demand_filtered(
            &c.query(), &historic, q, spec, |_| true,
        ), Err(Error::Read(ReadError::BeyondFrontier { .. }))));
        drop(db);
        assert_eq!(pin.beacon_search_graph_demand_filtered(
            &c.query(), &options(), q, spec, |id| id == HIGH,
        ).unwrap(), before);
        let db = Database::open_with_vfs(&c.commit(), vfs, &path, keys()).await.unwrap();
        historic.as_of = Some(at);
        assert_eq!(db.beacon_search_graph_demand_filtered(
            &c.query(), &historic, q, spec, |id| id == HIGH,
        ).unwrap(), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_sources_do_not_change_visible_work_scratch_or_node_admissions() {
    let ((), report) = run_async_under_lab(0xbeac_5104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut clean = Database::open_memory(&c.commit(), keys()).await.unwrap();
        let mut full = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut clean, &c.commit(), 0, false).await;
        seed(&mut full, &c.commit(), 0, true).await;
        let q = GraphHybridQuery::graph_only(4, 4, 1, 60).unwrap();
        let mut spec = expansion(&[VId(0), VId(90)]);
        spec.relation = None;
        let run = |db: &Database<MemVfs>| {
            let work = RefCell::new(WorkBudget::new(100_000));
            let nodes = Cell::new(0);
            let mut poll = || Ok(());
            let (rows, scratch) = evaluate_filtered(
                &db.snapshot, db.frontier().unwrap(), &options(), q, spec,
                &work, Scan::Unmetered(&mut poll),
                |row| {
                    if row.labels.binary_search(&LabelId(1)).is_err() { return Ok(false); }
                    nodes.set(nodes.get() + 1);
                    Ok(true)
                },
                |label| label == LabelId(1), |relation| relation == R, || Ok(()),
                |id| { assert_ne!(id, VId(90), "hidden endpoint escaped admission"); true },
            ).unwrap();
            (rows, scratch, nodes.get(), 100_000 - work.into_inner().remaining())
        };
        let expected = run(&clean);
        assert_eq!(endpoints(&expected.0), [(VId(2), 1), (HIGH, 2)]);
        assert_eq!(expected.2, 3);
        assert_eq!(run(&full), expected);
        let mut unscoped = options();
        unscoped.vertex_label = None;
        let actual = full.beacon_search_graph_demand_filtered(
            &c.query(), &unscoped, q, spec, |_| true,
        ).unwrap();
        assert_ne!(actual, expected.0, "the hidden fixture must affect an unrestricted read");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn work_and_shared_scratch_boundaries_refuse_without_partial_results() {
    #[derive(Default)]
    struct Cut { calls: usize, stop: usize }
    impl WorkControl for Cut {
        fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
            if self.calls >= self.stop { return Err(BeaconError::Cancelled); }
            self.calls += 1;
            if self.calls == self.stop { Err(BeaconError::Cancelled) } else { Ok(()) }
        }
    }
    let ((), report) = run_async_under_lab(0xbeac_5105, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit(), 0, false).await;
        let at = db.frontier().unwrap();
        let spec = expansion(&[VId(0)]);
        for k in [0, 1] {
            for accept in [false, true] {
                let q = GraphHybridQuery::graph_only(k, 1, 1, 60).unwrap();
                let run = |stop| {
                    let work = RefCell::new(Cut { calls: 0, stop });
                    let result = evaluate_filtered(
                        &db.snapshot, at, &options(), q, spec, &work, Scan::Metered,
                        |_| Ok(true), |_| true, |_| true, || Ok(()), |id| accept && id == HIGH,
                    );
                    (result, work.into_inner().calls)
                };
                let (expected, calls) = run(usize::MAX);
                assert_eq!(expected.unwrap().0.len(), usize::from(accept) * k);
                for stop in 1..=calls {
                    let (result, seen) = run(stop);
                    assert!(matches!(result, Err(BeaconError::Cancelled)), "k={k} cut={stop}");
                    assert_eq!(seen, stop);
                }
                let work = RefCell::new(WorkBudget::new(100_000));
                let (expected, scratch) = evaluate_filtered(
                    &db.snapshot, at, &options(), q, spec, &work, Scan::Metered,
                    |_| Ok(true), |_| true, |_| true, || Ok(()), |id| accept && id == HIGH,
                ).unwrap();
                let units = 100_000 - work.into_inner().remaining();
                let mut limited = options();
                limited.policy.max_source_scratch = usize::try_from(scratch).unwrap();
                limited.policy.max_work_units = units;
                assert_eq!(db.beacon_search_graph_demand_filtered(
                    &c.query(), &limited, q, spec, |id| accept && id == HIGH,
                ).unwrap(), expected);
                limited.policy.max_source_scratch -= 1;
                assert!(db.beacon_search_graph_demand_filtered(
                    &c.query(), &limited, q, spec, |_| false,
                ).is_err());
                limited.policy.max_source_scratch += 1;
                limited.policy.max_work_units -= 1;
                assert!(db.beacon_search_graph_demand_filtered(
                    &c.query(), &limited, q, spec, |id| accept && id == HIGH,
                ).is_err());
            }
        }
        assert_eq!(db.frontier().unwrap(), at);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_requests_and_visitation_limits_are_not_hidden_by_empty_outputs() {
    let ((), report) = run_async_under_lab(0xbeac_5106, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit(), 0, false).await;
        let mut q = GraphHybridQuery::graph_only(0, 1, 1, 60).unwrap();
        let mut spec = expansion(&[VId(0)]);
        spec.limits.max_visited_vertices = 2;
        assert!(matches!(db.beacon_search_graph_demand_filtered(
            &c.query(), &options(), q, spec, |_| false,
        ), Err(Error::Index(BeaconError::ResourceLimit {
            resource: "expansion visited vertices", limit: 2,
        }))));
        q.retrieval.profile = ExactRrfProfile::default();
        let work = RefCell::new(WorkBudget::new(100_000));
        assert!(matches!(evaluate_filtered(
            &db.snapshot, db.frontier().unwrap(), &options(), q, spec, &work,
            Scan::Metered, |_| panic!("mixed request accessed a source"),
            |_| true, |_| true, || Ok(()), |_| false,
        ), Err(BeaconError::InvalidQuery(_))));
        q = GraphHybridQuery::graph_only(0, 1, 1, 60).unwrap();
        q.graph_weight = 0;
        assert!(db.beacon_search_graph_demand_filtered(
            &c.query(), &options(), q, spec, |_| false,
        ).is_err());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
