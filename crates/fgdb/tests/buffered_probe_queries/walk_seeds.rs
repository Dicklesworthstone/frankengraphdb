//! Identity-bound external paths use point roots, retaining the native path
//! engine and complete-left barrier. Enumerate real edge occurrences below
//! independently of the production cursor and its admission strategy.

use super::*;
use fgdb_gql::algebra::GraphPath;
use std::collections::BTreeMap;

const ROOT: fgdb_delta_types::LabelId = fgdb_delta_types::LabelId(11);

fn roots() -> [VId; 5] {
    [IDS[2], IDS[0], IDS[2], IDS[1], VId(13)]
}

fn parameters() -> GqlParameters {
    let mut values: Vec<_> = roots().into_iter().map(GraphValue::Vertex).collect();
    values.extend([
        GraphValue::Vertex(VId(7)),
        GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::Scalar(CanonicalScalar::Int(42)),
    ]);
    GqlParameters::new().with_list("ids", values).unwrap()
}

async fn fixture(contexts: &PurposeContexts, vfs: &MemVfs) -> Database<MemVfs> {
    let mut db = seed(contexts, vfs).await;
    let mut extra = WriteBatch::new(R);
    for root in IDS {
        extra.set_vertex_label(root, ROOT, true);
    }
    extra.create_vertex(VId(13), vec![ROOT], vec![(P, CanonicalScalar::Int(13))]);
    for id in 100..164 {
        extra.create_vertex(VId(id), vec![], vec![]);
    }
    assert_eq!(
        db.write(&contexts.commit(), extra).await.unwrap(),
        CommitSeq(2)
    );
    db
}

fn value(vid: VId, cut: u64) -> GraphValue {
    let value = if vid == IDS[2] && cut == 3 {
        33
    } else {
        IDS.iter()
            .position(|id| *id == vid)
            .map_or(13, |at| at as i64 + 1)
    };
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn oracle(mode: &str, direction: usize, cut: u64) -> Vec<Vec<u8>> {
    let mut rows = Vec::new();
    for root in roots() {
        if cut == 3 && (root == VId(13) || root == IDS[1]) {
            continue;
        }
        // Build all finite edge sequences first, then check each completed
        // sequence. This deliberately shares no traversal state or pruning.
        let mut frontier = vec![Vec::<(EId, VId)>::new()];
        let mut paths = Vec::new();
        for depth in 0..=2 {
            for steps in &frontier {
                let vertices: Vec<_> = std::iter::once(root)
                    .chain(steps.iter().map(|step| step.1))
                    .collect();
                let valid = match mode {
                    "TRAIL" => steps
                        .iter()
                        .enumerate()
                        .all(|(at, step)| !steps[..at].iter().any(|prior| prior.0 == step.0)),
                    "ACYCLIC" => vertices
                        .iter()
                        .enumerate()
                        .all(|(at, vid)| !vertices[..at].contains(vid)),
                    "SIMPLE" => vertices.iter().enumerate().all(|(at, vid)| {
                        !vertices[..at].contains(vid) || (at + 1 == vertices.len() && *vid == root)
                    }),
                    "WALK" | "ANY SHORTEST WALK" => true,
                    _ => unreachable!(),
                };
                if valid {
                    paths.push(GraphPath::new(root, steps.clone().into_boxed_slice()));
                }
            }
            if depth == 2 {
                break;
            }
            let mut next = Vec::new();
            for steps in frontier {
                let endpoint = steps.last().map_or(root, |step| step.1);
                for (eid, from, to, _) in EDGES {
                    if cut == 3 && eid == 2 {
                        continue;
                    }
                    for (from, to) in orient(from, to, direction) {
                        if IDS[from] == endpoint {
                            let mut extended = steps.clone();
                            extended.push((EId(eid), IDS[to]));
                            next.push(extended);
                        }
                    }
                }
            }
            frontier = next;
        }
        if mode == "ANY SHORTEST WALK" {
            let mut shortest = BTreeMap::<VId, GraphPath>::new();
            for path in paths {
                let endpoint = path.steps().last().map_or(root, |step| step.1);
                if shortest
                    .get(&endpoint)
                    .is_none_or(|prior| (path.len(), &path) < (prior.len(), prior))
                {
                    shortest.insert(endpoint, path);
                }
            }
            paths = shortest.into_values().collect();
        }
        for path in paths {
            let endpoint = path.steps().last().map_or(root, |step| step.1);
            rows.push(vec![
                GraphValue::Vertex(root),
                GraphValue::Vertex(root),
                GraphValue::Vertex(endpoint),
                GraphValue::Path(path),
                value(root, cut),
                value(endpoint, cut),
            ]);
        }
    }
    rows.sort();
    rows.into_iter()
        .map(|row| {
            GraphValueRow::from_owned_values(row)
                .canonical_bytes()
                .unwrap()
        })
        .collect()
}

fn path_pattern(mode: &str, direction: usize) -> String {
    let atom = match direction {
        0 => "-[:R*0..2]->(b)",
        1 => "<-[:R*0..2]-(b)",
        _ => "-[:R*0..2]-(b)",
    };
    format!(
        "MATCH REPEATABLE ELEMENTS {mode} path=(a:Root){atom} \
         WHERE a=wanted AND a.p>=1"
    )
}

fn record_refusal(error: NativeSpoolError, required: u64) {
    assert!(
        matches!(error, NativeSpoolError::BufferedExecute(ref error)
            if matches!(**error, GqlQueryError::Rows(ref error)
                if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                    && error.limit == required - 1 && error.observed == required)),
        "{error:?}"
    );
}

#[test]
fn seeded_external_paths_match_literal_routes_across_modes_directions_and_history() {
    let ((), report) = run_async_under_lab(0xe6_74, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = fixture(&contexts, &vfs).await;
        let mut edit = WriteBatch::new(R);
        edit.delete_vertex(VId(13));
        edit.delete_edge(EId(2));
        edit.set_vertex_label(IDS[1], ROOT, false);
        edit.set_vertex_property(IDS[2], P, Some(CanonicalScalar::Int(33)));
        assert_eq!(
            db.write(&contexts.commit(), edit).await.unwrap(),
            CommitSeq(3)
        );
        db.compact(&contexts.commit()).await.unwrap();
        let parameters = parameters();
        let source_pool = memory();
        let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
        for (mode, direction, cut) in [
            ("WALK", 0, 2),
            ("WALK", 0, 3),
            ("WALK", 1, 2),
            ("WALK", 2, 3),
            ("TRAIL", 2, 2),
            ("ACYCLIC", 0, 3),
            ("SIMPLE", 2, 2),
            ("ANY SHORTEST WALK", 0, 2),
            ("ANY SHORTEST WALK", 1, 3),
            ("ANY SHORTEST WALK", 2, 2),
        ] {
            let pattern = path_pattern(mode, direction);
            let text = format!(
                "UNWIND $ids AS wanted {pattern} FOR SYSTEM_TIME AS OF SEQ {cut} \
                 RETURN ALL wanted,a,b,path,a.p AS root_value,b.p AS end_value \
                 ORDER BY wanted,a,b,path"
            );
            let expected = oracle(mode, direction, cut);
            let (rows, snapshot, stats) = ordered_parameters(
                &mut view,
                &cx,
                &text,
                &parameters,
                GqlQueryPolicy::new(64, 1000, SORT_WORK, SORT_WORK),
                8,
            )
            .await
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(rows, expected, "{text}");
            assert_eq!(snapshot, CommitSeq(cut));
            let (rows, _, fallback) =
                ordered_parameters(&mut view, &cx, &text, &parameters, policy(), 1)
                    .await
                    .unwrap();
            assert_eq!(rows, expected, "bounded admission fallback: {text}");
            assert_eq!(
                fallback.snapshot_records,
                stats.snapshot_records + 63,
                "68 historical root candidates become five distinct point roots; \
                 identical reached incidence retains its original accounting"
            );
            let error = ordered_parameters(
                &mut view,
                &cx,
                &format!("{text} LIMIT 0"),
                &parameters,
                GqlQueryPolicy::new(stats.snapshot_records - 1, 0, SORT_WORK, SORT_WORK),
                8,
            )
            .await
            .unwrap_err();
            record_refusal(error, stats.snapshot_records);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn seeded_external_shortest_apply_keeps_occurrences_and_the_complete_left_barrier() {
    let ((), report) = run_async_under_lab(0xe6_75, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _db = fixture(&contexts, &vfs).await;
        let source_pool = memory();
        let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
        let parameters = parameters();
        let pattern = path_pattern("ANY SHORTEST WALK", 0);
        let text = format!(
            "UNWIND $ids AS wanted CALL (wanted) {{ {pattern} \
             RETURN ALL a,b,path,a.p AS root_value,b.p AS end_value }} \
             RETURN ALL wanted,a,b,path,root_value,end_value ORDER BY wanted,a,b,path"
        );
        let (rows, snapshot, stats) = ordered_parameters(
            &mut view,
            &cx,
            &text,
            &parameters,
            GqlQueryPolicy::new(64, 1000, SORT_WORK, SORT_WORK),
            1,
        )
        .await
        .unwrap();
        assert_eq!(rows, oracle("ANY SHORTEST WALK", 0, 2));
        assert_eq!(snapshot, CommitSeq(2));
        let error = ordered_parameters(
            &mut view,
            &cx,
            &format!("{text} LIMIT 0"),
            &parameters,
            GqlQueryPolicy::new(stats.snapshot_records - 1, 0, SORT_WORK, SORT_WORK),
            1,
        )
        .await
        .unwrap_err();
        record_refusal(error, stats.snapshot_records);
        let groups = GqlParameters::new()
            .with_list(
                "groups",
                vec![
                    GraphValue::List(vec![GraphValue::Vertex(IDS[2])].into_boxed_slice()),
                    GraphValue::Scalar(CanonicalScalar::Int(2)),
                ],
            )
            .unwrap();
        let before = view.buffer_stats();
        let late_error = format!(
            "UNWIND $groups AS items UNWIND items AS wanted {pattern} RETURN a,b,path LIMIT 0"
        );
        let error = ordered_parameters(&mut view, &cx, &late_error, &groups, policy(), 8)
            .await
            .unwrap_err();
        assert!(
            matches!(error, NativeSpoolError::SetExecution(fgdb_gql::GraphSetExecutionError::Projection { error, .. })
                if error.kind == fgdb_gql::GraphIntegerErrorKind::IncompatibleOperands),
            "{error:?}"
        );
        assert_eq!(
            view.buffer_stats(),
            before,
            "left completion precedes every root read"
        );
        let empty = GqlParameters::new().with_list("ids", vec![]).unwrap();
        let (rows, _, stats) = ordered_parameters(
            &mut view,
            &cx,
            &text,
            &empty,
            GqlQueryPolicy::new(0, 0, SORT_WORK, SORT_WORK),
            1,
        )
        .await
        .unwrap();
        assert!(rows.is_empty());
        assert_eq!(stats.snapshot_records, 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
