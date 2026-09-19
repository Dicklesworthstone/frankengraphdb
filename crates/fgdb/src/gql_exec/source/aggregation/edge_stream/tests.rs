use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

type EdgeImage = (
    EId,
    VId,
    RelationId,
    VId,
    Vec<(PropertyKeyId, CanonicalScalar)>,
);
fn read_source(
    view: EmbeddedReadView,
    cx: &QueryCx,
    as_of: CommitSeq,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), usize>,
) -> Result<Vec<EdgeImage>, EdgeScanSourceError<ReadError, usize>> {
    let mut source = SnapshotEdgeSource {
        view,
        cx,
        as_of,
        after: None,
    };
    let mut rows = Vec::new();
    while let Some(eid) = source.next_edge(control)? {
        let Some(row) = source.edge(eid, control)? else {
            continue;
        };
        for vid in [row.source, row.target] {
            let vertex = source.vertex(vid, control)?.expect("live visible endpoint");
            assert!(vertex.labels.windows(2).all(|pair| pair[0] < pair[1]));
        }
        rows.push((
            eid,
            row.source,
            row.relation,
            row.target,
            row.properties.to_vec(),
        ));
    }
    Ok(rows)
}

#[test]
fn history_predecessors_equal_authoritative_rows_and_every_source_boundary_propagates_refusal() {
    let ((), report) = run_async_under_lab(0xed6e_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let keys = crate::DatabaseKeys::new(
            [0xa7; 32],
            DatabaseSecurityNamespaceId([0xa8; 32]),
            [0xa9; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let relation = RelationId(7);
        let key = PropertyKeyId(9);
        let mut batch = crate::WriteBatch::new(relation);
        for id in [VId(0), VId(u128::MAX)] {
            batch.create_vertex(id, vec![], vec![(key, CanonicalScalar::Int(1))]);
        }
        for eid in [EId(0), EId(1), EId(u128::MAX)] {
            batch.add_edge(
                eid,
                VId(0),
                VId(u128::MAX),
                vec![(key, CanonicalScalar::Int(3))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let old = db.read_session().unwrap();
        for round in 0..4 {
            let mut update = crate::WriteBatch::new(relation);
            update.set_edge_property(EId(0), key, Some(CanonicalScalar::Int(round + 4)));
            update.set_vertex_property(VId(0), key, Some(CanonicalScalar::Int(round + 5)));
            if round == 1 {
                update.delete_edge(EId(1));
            }
            db.write(&commit, update).await.unwrap();
        }
        let view = db.read_session().unwrap();
        for cut in 0..=db.frontier().unwrap().0 {
            let as_of = CommitSeq(cut);
            let mut expected: Vec<_> = db
                .edges_at(as_of)
                .unwrap()
                .into_iter()
                .map(|row| {
                    (
                        row.entry.eid,
                        row.entry.src,
                        row.entry.relation,
                        row.entry.dst,
                        row.props,
                    )
                })
                .collect();
            expected.sort_by_key(|row| row.0);
            let mut total = 0;
            let actual = read_source(view.clone(), &cx, as_of, &mut |_| {
                total += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(actual, expected);
            for stop in 1..=total {
                let mut at = 0;
                let refused = read_source(view.clone(), &cx, as_of, &mut |_| {
                    at += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                });
                assert!(
                    matches!(refused, Err(EdgeScanSourceError::Control(value)) if value == stop)
                );
                assert_eq!(at, stop);
                assert_eq!(
                    read_source(view.clone(), &cx, as_of, &mut |_| Ok(())).unwrap(),
                    expected
                );
            }
        }
        let expected_old = old.edges().unwrap();
        let old_cut = old.frontier();
        let old_rows = read_source(old, &cx, old_cut, &mut |_| Ok(())).unwrap();
        assert_eq!(old_rows.len(), expected_old.len());
        assert!(
            old_rows
                .iter()
                .all(|row| row.4 == vec![(key, CanonicalScalar::Int(3))])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
