//! External identity-bound fixed-edge sources retain native orientations and
//! multiplicity while routing a single globally ordered selected root merge.
//! Expected rows enumerate the literal graph product independently of cursors.

use super::*;

const ROOT: fgdb_delta_types::LabelId = fgdb_delta_types::LabelId(11);

async fn fixture(contexts: &PurposeContexts, vfs: &MemVfs) -> Database<MemVfs> {
    let mut db = seed(contexts, vfs).await;
    let mut extra = WriteBatch::new(R);
    for root in IDS {
        extra.set_vertex_label(root, ROOT, true);
    }
    extra.create_vertex(VId(13), vec![ROOT], vec![(P, CanonicalScalar::Int(13))]);
    for id in 100..164 {
        extra.create_vertex(VId(id), vec![], vec![]);
        extra.add_edge(EId(id), VId(id), VId(id), vec![]);
    }
    assert_eq!(
        db.write(&contexts.commit(), extra).await.unwrap(),
        CommitSeq(2)
    );
    let mut edit = WriteBatch::new(R);
    edit.delete_vertex(VId(13));
    edit.delete_edge(EId(2));
    edit.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(17)));
    edit.set_vertex_label(IDS[1], ROOT, false);
    edit.set_vertex_property(IDS[2], P, Some(CanonicalScalar::Int(33)));
    assert_eq!(
        db.write(&contexts.commit(), edit).await.unwrap(),
        CommitSeq(3)
    );
    db.compact(&contexts.commit()).await.unwrap();
    db
}

fn parameters(roots: &[VId]) -> GqlParameters {
    let mut values: Vec<_> = roots.iter().copied().map(GraphValue::Vertex).collect();
    values.extend([
        GraphValue::Vertex(VId(7)),
        GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::Scalar(CanonicalScalar::Int(42)),
    ]);
    GqlParameters::new().with_list("ids", values).unwrap()
}

fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn vertex_value(index: usize, cut: u64) -> GraphValue {
    scalar(if index == 2 && cut == 3 {
        33
    } else {
        index as i64 + 1
    })
}

#[derive(Clone, Copy)]
struct Shape {
    first: usize,
    second: Option<usize>,
    branch: bool,
    repeatable: bool,
    cut: u64,
}

impl Shape {
    fn pattern(self) -> String {
        let mut pattern = format!(
            "MATCH {}(a:Root){}",
            if self.repeatable {
                "REPEATABLE ELEMENTS "
            } else {
                ""
            },
            atom("r", "b", self.first),
        );
        if let Some(second) = self.second {
            if self.branch {
                pattern.push_str(",(a)");
            }
            pattern.push_str(&atom("s", "c", second));
        }
        pattern.push_str(" WHERE a=wanted AND a.p>=1");
        pattern
    }

    fn query(self, distinct: bool) -> String {
        let extra = if self.second.is_some() {
            ",c,s,c.p AS cp,s.p AS sp"
        } else {
            ""
        };
        let order = if self.second.is_some() {
            ",c,s,cp,sp"
        } else {
            ""
        };
        format!(
            "UNWIND $ids AS wanted {} FOR SYSTEM_TIME AS OF SEQ {} \
             RETURN {} wanted,a,b,r,a.p AS ap,b.p AS bp,r.p AS rp{extra} \
             ORDER BY wanted,a,b,r,ap,bp,rp{order}",
            self.pattern(),
            self.cut,
            if distinct { "DISTINCT" } else { "ALL" },
        )
    }

    fn oracle(self, roots: &[VId], distinct: bool) -> Vec<Vec<u8>> {
        let edges: Vec<_> = EDGES
            .into_iter()
            .filter(|(eid, _, _, _)| self.cut != 3 || *eid != 2)
            .map(|(eid, from, to, value)| {
                (
                    eid,
                    from,
                    to,
                    if eid == 0 && self.cut == 3 { 17 } else { value },
                )
            })
            .collect();
        let mut rows = Vec::new();
        for &wanted in roots {
            for &(eid, from, to, value) in &edges {
                for (a, b) in orient(from, to, self.first) {
                    if IDS[a] != wanted || (self.cut == 3 && a == 1) {
                        continue;
                    }
                    let prefix = vec![
                        GraphValue::Vertex(wanted),
                        GraphValue::Vertex(IDS[a]),
                        GraphValue::Vertex(IDS[b]),
                        GraphValue::Edge(EId(eid)),
                        vertex_value(a, self.cut),
                        vertex_value(b, self.cut),
                        scalar(value),
                    ];
                    let Some(direction) = self.second else {
                        rows.push(prefix);
                        continue;
                    };
                    for &(child, from, to, value) in &edges {
                        if !self.repeatable && child == eid {
                            continue;
                        }
                        for (start, c) in orient(from, to, direction) {
                            if start != if self.branch { a } else { b } {
                                continue;
                            }
                            let mut row = prefix.clone();
                            row.extend([
                                GraphValue::Vertex(IDS[c]),
                                GraphValue::Edge(EId(child)),
                                vertex_value(c, self.cut),
                                scalar(value),
                            ]);
                            rows.push(row);
                        }
                    }
                }
            }
        }
        rows.sort();
        if distinct {
            rows.dedup();
        }
        rows.into_iter()
            .map(|row| {
                GraphValueRow::from_owned_values(row)
                    .canonical_bytes()
                    .unwrap()
            })
            .collect()
    }
}

#[test]
fn seeded_fixed_edges_and_branches_match_literal_bags_across_directions_and_history() {
    let ((), report) = run_async_under_lab(0xe6_77, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _db = fixture(&contexts, &vfs).await;
        let source_pool = memory();
        let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
        let roots = [IDS[2], IDS[0], IDS[2], IDS[1], VId(13)];
        let parameters = parameters(&roots);
        for (first, second, branch, repeatable, cut) in [
            (0, None, false, false, 2),
            (1, None, false, false, 3),
            (2, None, false, false, 2),
            (2, None, false, false, 3),
            (0, Some(0), false, false, 2),
            (1, Some(2), false, true, 3),
            (2, Some(0), true, false, 2),
            (0, Some(1), true, true, 3),
        ] {
            let shape = Shape {
                first,
                second,
                branch,
                repeatable,
                cut,
            };
            let text = shape.query(false);
            let expected = shape.oracle(&roots, false);
            let (rows, snapshot, stats) = ordered_parameters(
                &mut view,
                &cx,
                &text,
                &parameters,
                GqlQueryPolicy::new(256, 1000, SORT_WORK, SORT_WORK),
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
            assert_eq!(rows, expected, "bounded selection fallback: {text}");
            assert_eq!(
                fallback.snapshot_records,
                stats.snapshot_records + 64,
                "all five core EIds remain one ordered root stream; 64 unrelated \
                 historical roots disappear without changing reached child candidates",
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
            assert!(
                matches!(error, NativeSpoolError::BufferedExecute(ref error)
                if matches!(**error, GqlQueryError::Rows(ref error)
                    if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                        && error.limit == stats.snapshot_records - 1
                        && error.observed == stats.snapshot_records)),
                "{error:?}"
            );
            let (rows, _, _) = ordered_parameters(
                &mut view,
                &cx,
                &format!("{} SKIP 1 LIMIT 3", shape.query(true)),
                &parameters,
                policy(),
                8,
            )
            .await
            .unwrap();
            assert_eq!(
                rows,
                shape
                    .oracle(&roots, true)
                    .into_iter()
                    .skip(1)
                    .take(3)
                    .collect::<Vec<_>>()
            );
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_edge_orientations_preserve_partial_domains_and_complete_left_failures() {
    let ((), report) = run_async_under_lab(0xe6_78, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _db = fixture(&contexts, &vfs).await;
        let source_pool = memory();
        let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
        let roots = [IDS[2], IDS[0], IDS[2]];
        let values = parameters(&roots);
        for first in 0..=2 {
            let shape = Shape {
                first,
                second: Some(2),
                branch: false,
                repeatable: true,
                cut: 2,
            };
            let text = shape.query(false);
            let (rows, _, stats) = ordered_parameters(&mut view, &cx, &text, &values, policy(), 8)
                .await
                .unwrap();
            assert_eq!(rows, shape.oracle(&roots, false));
            let (fallback, _, ordinary) =
                ordered_parameters(&mut view, &cx, &text, &values, policy(), 1)
                    .await
                    .unwrap();
            assert_eq!(rows, fallback);
            assert!(ordinary.snapshot_records >= stats.snapshot_records + 64);
        }
        let shape = Shape {
            first: 0,
            second: Some(0),
            branch: true,
            repeatable: false,
            cut: 2,
        };
        let groups = GqlParameters::new()
            .with_list(
                "groups",
                vec![
                    GraphValue::List(vec![GraphValue::Vertex(IDS[0])].into_boxed_slice()),
                    GraphValue::Scalar(CanonicalScalar::Int(2)),
                ],
            )
            .unwrap();
        let before = view.buffer_stats();
        let text = format!(
            "UNWIND $groups AS items UNWIND items AS wanted {} RETURN a,b,c,r,s LIMIT 0",
            shape.pattern(),
        );
        let error = ordered_parameters(&mut view, &cx, &text, &groups, policy(), 8)
            .await
            .unwrap_err();
        assert!(
            matches!(error, NativeSpoolError::SetExecution(
            fgdb_gql::GraphSetExecutionError::Projection { error, .. })
                if error.kind == fgdb_gql::GraphIntegerErrorKind::IncompatibleOperands),
            "{error:?}"
        );
        assert_eq!(
            view.buffer_stats(),
            before,
            "no right routing or source reads before left completion"
        );
        let empty = GqlParameters::new().with_list("ids", vec![]).unwrap();
        let (rows, _, stats) = ordered_parameters(
            &mut view,
            &cx,
            &shape.query(false),
            &empty,
            GqlQueryPolicy::new(0, 0, SORT_WORK, SORT_WORK),
            8,
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
