//! Real Chronicle -> edge input -> arranged join -> DISTINCT -> aggregate -> sink.
//! Expected results come from public historical storage scans, never from the
//! input adapter, a query evaluator, or another incremental circuit.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::zset::aggregate::{AggregateDelta, IncrementalAggregate};
use fgdb_delta_types::zset::committed::{CommittedEdgeInput, EdgeInputError};
use fgdb_delta_types::zset::incremental::{IncrementalDistinct, IncrementalJoin};
use fgdb_delta_types::{
    LimbLimit, LocalDeltaBatchIndex, PropertyKeyId, RelationId, ZSet, ZSetEvent, ZWeight,
};
use fgdb_types::{
    BranchId, CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, GraphId,
    PurposeContexts, VId,
};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const LIMBS: LimbLimit = LimbLimit::new(16);
type Pair = (VId, VId);
type Summary = (i128, i128, Option<i128>, Option<i128>, Option<i128>);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}

#[derive(Debug, PartialEq, Eq)]
struct Pipeline {
    input: CommittedEdgeInput,
    join: IncrementalJoin<VId, VId, VId>,
    bag: ZSet<Pair>,
    distinct: IncrementalDistinct<Pair>,
    aggregate: IncrementalAggregate<VId>,
    sink: AggregateDelta<VId>,
}
impl Pipeline {
    fn new() -> Self {
        Self {
            input: CommittedEdgeInput::new(GraphId(1), BranchId(1)),
            join: IncrementalJoin::new(),
            bag: ZSet::new(),
            distinct: IncrementalDistinct::new(),
            aggregate: IncrementalAggregate::new(),
            sink: ZSet::new(),
        }
    }
    fn tick(
        &mut self,
        source: &LocalDeltaBatchIndex,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), usize>,
    ) -> Result<Option<CommitSeq>, ()> {
        let Some(input) = self
            .input
            .prepare_next(source, LIMBS, control)
            .map_err(|_| ())?
        else {
            return Ok(None);
        };
        let left = input
            .delta()
            .filter(|tuple| Ok(tuple.0 == R), LIMBS, control)
            .map_err(|_| ())?
            .map(|&(_, source, via)| Ok((via, source)), LIMBS, control)
            .map_err(|_| ())?;
        let right = input
            .delta()
            .filter(|tuple| Ok(tuple.0 == S), LIMBS, control)
            .map_err(|_| ())?
            .map(|&(_, via, target)| Ok((via, target)), LIMBS, control)
            .map_err(|_| ())?;
        let joined = self
            .join
            .prepare(&left, &right, LIMBS, control)
            .map_err(|_| ())?;
        let projected = joined
            .delta()
            .map(|&(_, source, target)| Ok((source, target)), LIMBS, control)
            .map_err(|_| ())?;
        let bag = self
            .bag
            .prepare_update(&projected, LIMBS, control)
            .map_err(|_| ())?;
        let distinct = self
            .distinct
            .prepare(&projected, LIMBS, control)
            .map_err(|_| ())?;
        let values = distinct
            .delta()
            .map(
                |&(source, target)| {
                    i128::try_from(target.0)
                        .map(|value| (source, Some(value)))
                        .map_err(|_| usize::MAX)
                },
                LIMBS,
                control,
            )
            .map_err(|_| ())?;
        let aggregate = self
            .aggregate
            .prepare(&values, LIMBS, control)
            .map_err(|_| ())?;
        let sink = self
            .sink
            .prepare_update(aggregate.delta(), LIMBS, control)
            .map_err(|_| ())?;
        control(ZSetEvent::Work).map_err(|_| ())?;
        let seq = input.commit_seq();
        bag.commit();
        sink.commit();
        let _ = aggregate.commit();
        let _ = distinct.commit();
        let _ = joined.commit();
        let _ = input.commit();
        Ok(Some(seq))
    }
    fn replay(source: &LocalDeltaBatchIndex) -> Self {
        let mut pipeline = Self::new();
        while pipeline.tick(source, &mut allow).unwrap().is_some() {}
        pipeline
    }
    fn bag_rows(&self) -> BTreeMap<Pair, i128> {
        self.bag
            .iter()
            .map(|(pair, weight)| (*pair, weight.to_i128().unwrap()))
            .collect()
    }
    fn summaries(&self) -> BTreeMap<VId, Summary> {
        self.sink
            .iter()
            .map(|((source, values), weight)| {
                assert_eq!(weight, &ZWeight::ONE);
                assert_eq!(self.aggregate.get(source), Some(values.as_ref()));
                (
                    *source,
                    (
                        values.count_rows().to_i128().unwrap(),
                        values.count_values().to_i128().unwrap(),
                        values.sum().map(|value| value.to_i128().unwrap()),
                        values.minimum(),
                        values.maximum(),
                    ),
                )
            })
            .collect()
    }
}

fn oracle(db: &Database<MemVfs>, seq: CommitSeq) -> (BTreeMap<Pair, i128>, BTreeMap<VId, Summary>) {
    let edges = db.edges_at(seq).unwrap();
    let mut bag = BTreeMap::new();
    let mut targets = BTreeMap::<VId, BTreeSet<i128>>::new();
    for left in &edges {
        if left.entry.relation != R {
            continue;
        }
        for right in &edges {
            if right.entry.relation == S && left.entry.dst == right.entry.src {
                *bag.entry((left.entry.src, right.entry.dst)).or_default() += 1;
                targets
                    .entry(left.entry.src)
                    .or_default()
                    .insert(i128::try_from(right.entry.dst.0).unwrap());
            }
        }
    }
    let summary = targets
        .into_iter()
        .map(|(source, targets)| {
            (
                source,
                (
                    targets.len() as i128,
                    targets.len() as i128,
                    Some(targets.iter().sum()),
                    targets.first().copied(),
                    targets.last().copied(),
                ),
            )
        })
        .collect();
    (bag, summary)
}
fn check(db: &Database<MemVfs>, pipeline: &Pipeline) {
    let expected = oracle(db, pipeline.input.frontier());
    assert_eq!(pipeline.bag_rows(), expected.0);
    assert_eq!(pipeline.summaries(), expected.1);
    assert_eq!(
        pipeline.input.edge_count(),
        db.edges_at(pipeline.input.frontier()).unwrap().len()
    );
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(RelationId(9));
    for id in 1..=8 {
        vertices.create_vertex(VId(id), vec![], vec![]);
    }
    let mut left = WriteBatch::new(R);
    for (id, via) in [(11, 2), (12, 2), (13, 3)] {
        left.add_edge(EId(id), VId(1), VId(via), vec![]);
    }
    let mut right = WriteBatch::new(S);
    for (id, via, target) in [(21, 2, 4), (22, 2, 4), (23, 3, 4), (24, 3, 5)] {
        right.add_edge(EId(id), VId(via), VId(target), vec![]);
    }
    db.write_atomic(cx, vec![vertices, right, left])
        .await
        .unwrap()
}

#[test]
fn committed_view_tracks_atomic_writes_cascades_and_recovered_history() {
    let ((), report) = run_async_under_lab(0xc017_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let first = seed(&mut db, &cx).await;
        assert_eq!(db.delta_since(CommitSeq(0)).unwrap().count(), 1);
        let old_source = db.delta_index().unwrap().clone();
        let pinned = db.read_session().unwrap();
        let mut pipeline = Pipeline::replay(db.delta_index().unwrap());
        check(&db, &pipeline);
        assert_eq!(
            pipeline.bag_rows(),
            BTreeMap::from([((VId(1), VId(4)), 5), ((VId(1), VId(5)), 1)])
        );
        let frozen = pipeline.sink.checked_clone(LIMBS, &mut allow).unwrap();

        let mut property_only = WriteBatch::new(R);
        property_only.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
        let seq = db.write(&cx, property_only).await.unwrap();
        assert_eq!(
            pipeline
                .tick(db.delta_index().unwrap(), &mut allow)
                .unwrap(),
            Some(seq)
        );
        assert_eq!(pipeline.sink, frozen);
        let mut replace = WriteBatch::new(R);
        replace.delete_edge(EId(11));
        replace.add_edge(EId(15), VId(1), VId(2), vec![]);
        let replacement = db.write(&cx, replace).await.unwrap();
        assert_eq!(
            pipeline
                .tick(db.delta_index().unwrap(), &mut allow)
                .unwrap(),
            Some(replacement)
        );
        assert_eq!(pipeline.sink, frozen);
        check(&db, &pipeline);

        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut left = WriteBatch::new(R);
        left.delete_edge(EId(12));
        let mut right = WriteBatch::new(S);
        right.delete_edge(EId(21));
        txn.write_atomic(&mut db, vec![left, right]).unwrap();
        assert!(
            pipeline
                .tick(db.delta_index().unwrap(), &mut allow)
                .unwrap()
                .is_none(),
            "staged writes must not enter committed input"
        );
        assert_eq!(pipeline.input.frontier(), replacement);
        let committed = txn.commit(&mut db, &cx).await.unwrap();
        assert_eq!(
            pipeline
                .tick(db.delta_index().unwrap(), &mut allow)
                .unwrap(),
            Some(committed)
        );
        check(&db, &pipeline);

        // One vertex deletion removes edges in both join inputs atomically.
        let mut delete = WriteBatch::new(R);
        delete.delete_vertex(VId(2));
        let retired = db.write(&cx, delete).await.unwrap();
        assert_eq!(
            pipeline
                .tick(db.delta_index().unwrap(), &mut allow)
                .unwrap(),
            Some(retired)
        );
        check(&db, &pipeline);
        assert_eq!(
            pipeline.bag_rows(),
            BTreeMap::from([((VId(1), VId(4)), 1), ((VId(1), VId(5)), 1)])
        );
        assert_eq!(pinned.edges().unwrap().len(), 7);
        assert_eq!(Pipeline::replay(&old_source).sink, frozen);
        assert_eq!(
            oracle(&db, first).0,
            Pipeline::replay(&old_source).bag_rows()
        );

        db.compact(&cx).await.unwrap();
        drop(db);
        let mut db = Database::open_with_vfs(&cx, vfs, &path, keys())
            .await
            .unwrap();
        assert!(
            pipeline
                .tick(db.delta_index().unwrap(), &mut allow)
                .unwrap()
                .is_none()
        );
        assert_eq!(Pipeline::replay(db.delta_index().unwrap()), pipeline);
        check(&db, &pipeline);
        // A same-commit new left AND right require the derivative cross term.
        let mut left = WriteBatch::new(R);
        left.add_edge(EId(30), VId(1), VId(6), vec![]);
        let mut right = WriteBatch::new(S);
        right.add_edge(EId(31), VId(6), VId(5), vec![]);
        let seq = db.write_atomic(&cx, vec![left, right]).await.unwrap();
        assert_eq!(
            pipeline
                .tick(db.delta_index().unwrap(), &mut allow)
                .unwrap(),
            Some(seq)
        );
        check(&db, &pipeline);
        assert_eq!(pipeline.bag_rows().get(&(VId(1), VId(5))), Some(&2));
        assert_eq!(
            pipeline.sink, frozen,
            "bag changed but distinct aggregate did not"
        );

        // Identical keys/frontiers do not authorize a different source history.
        let mut foreign = Database::open_memory(&cx, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(909), vec![], vec![]);
        foreign.write(&cx, batch).await.unwrap();
        let mut early = Pipeline::replay(&old_source);
        assert!(matches!(
            early
                .input
                .prepare_next(foreign.delta_index().unwrap(), LIMBS, &mut allow),
            Err(EdgeInputError::HistoryChanged { .. })
        ));
        assert_eq!(early, Pipeline::replay(&old_source));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_downstream_refusal_preserves_the_committed_cursor_and_all_operators() {
    let ((), report) = run_async_under_lab(0xc017_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let first = seed(&mut db, &cx).await;
        let before = db.delta_index().unwrap().clone();
        let mut left = WriteBatch::new(R);
        left.delete_edge(EId(11));
        left.add_edge(EId(40), VId(7), VId(8), vec![]);
        let mut right = WriteBatch::new(S);
        right.delete_edge(EId(21));
        right.add_edge(EId(41), VId(8), VId(4), vec![]);
        let next = db.write_atomic(&cx, vec![left, right]).await.unwrap();
        let mut expected = Pipeline::replay(&before);
        let mut total = 0;
        assert_eq!(
            expected
                .tick(db.delta_index().unwrap(), &mut |_| {
                    total += 1;
                    Ok(())
                })
                .unwrap(),
            Some(next)
        );
        check(&db, &expected);
        for stop in 1..=total {
            let mut state = Pipeline::replay(&before);
            let mut seen = 0;
            assert!(
                state
                    .tick(db.delta_index().unwrap(), &mut |_| {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    })
                    .is_err()
            );
            assert_eq!(seen, stop);
            assert_eq!(state.input.frontier(), first);
            assert_eq!(state, Pipeline::replay(&before));
            assert_eq!(
                state.tick(db.delta_index().unwrap(), &mut allow).unwrap(),
                Some(next)
            );
            assert_eq!(state, expected);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn repeated_real_commits_match_storage_scan_recomputation_at_every_sequence() {
    let ((), report) = run_async_under_lab(0xc017_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let mut pipeline = Pipeline::replay(db.delta_index().unwrap());
        let mut random = 13_u64;
        for tick in 0..40_u128 {
            let old = db.edges().unwrap();
            let mut batches = Vec::new();
            for (offset, relation) in [(0, R), (1, S)] {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let mut batch = WriteBatch::new(relation);
                batch.add_edge(
                    EId(100 + 2 * tick + offset),
                    VId(1 + u128::from(random % 8)),
                    VId(1 + u128::from((random >> 8) % 8)),
                    vec![],
                );
                if tick % 2 == 0 {
                    let candidates: Vec<_> = old
                        .iter()
                        .filter(|edge| edge.entry.relation == relation)
                        .collect();
                    if !candidates.is_empty() {
                        let victim = candidates[(random as usize) % candidates.len()];
                        batch.delete_edge(victim.entry.eid);
                    }
                }
                batches.push(batch);
            }
            let seq = db.write_atomic(&cx, batches).await.unwrap();
            assert_eq!(
                pipeline
                    .tick(db.delta_index().unwrap(), &mut allow)
                    .unwrap(),
                Some(seq)
            );
            check(&db, &pipeline);
            if tick % 10 == 9 {
                db.compact(&cx).await.unwrap();
                assert_eq!(Pipeline::replay(db.delta_index().unwrap()), pipeline);
            }
        }
        let mut replayed = Pipeline::new();
        while replayed
            .tick(db.delta_index().unwrap(), &mut allow)
            .unwrap()
            .is_some()
        {
            check(&db, &replayed);
        }
        assert_eq!(replayed, pipeline);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
