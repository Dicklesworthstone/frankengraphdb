//! Every real history/index/predicate/delivery checkpoint, using one view pin.
use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::RelationId;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::{cell::Cell, sync::Arc};

#[test]
fn vertex_and_probe_sources_share_one_pin_and_every_refusal_releases_it() {
    let ((), report) = run_async_under_lab(0x7670_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let keys = crate::DatabaseKeys::new(
            [0x91; 32],
            DatabaseSecurityNamespaceId([0x92; 32]),
            [0x93; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let relation = RelationId(1);
        let mut batch = crate::WriteBatch::new(relation);
        for i in 0..4 {
            batch.create_vertex(
                VId(i),
                vec![],
                vec![(
                    fgdb_delta_types::PropertyKeyId(1),
                    CanonicalScalar::Int(i as i64),
                )],
            );
        }
        for (id, a, b) in [(0, 0, 1), (1, 0, 2), (2, 2, 0)] {
            batch.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        let basis = db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        for anti in [false, true] {
            let input = format!(
                "MATCH (a) WHERE {}EXISTS {{ MATCH (a)-[:R]->(x) WHERE x.p > 1 }} RETURN a, a.p AS p",
                if anti { "NOT " } else { "" }
            );
            let pattern = PreparedGraphText::prepare(&input, |kind, _| match kind {
                GraphSymbolKind::Relation => Some(GraphSymbol::Relation(relation)),
                GraphSymbolKind::Property => {
                    Some(GraphSymbol::Property(fgdb_delta_types::PropertyKeyId(1)))
                }
                _ => None,
            })
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            let plan = VertexScanPlan::compile(pattern.plan()).unwrap();
            let wide = GqlQueryPolicy::new(1000, 1000, 100_000, 100_000);
            let calls = Cell::new(0);
            let mut good = VertexScanCursor::new(
                SnapshotVertexSource {
                    view: view.clone(),
                    cx: &cx,
                    as_of: basis,
                    after: None,
                },
                plan.clone(),
                wide,
                || {
                    calls.set(calls.get() + 1);
                    Ok::<_, usize>(())
                },
            );
            let expected = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            let stats = good.evaluator_stats();
            let total = calls.get();
            drop(good);
            for stop in 1..=total {
                let refs = Arc::strong_count(&view.snapshot);
                let calls = Cell::new(0);
                let mut cursor = VertexScanCursor::new(
                    SnapshotVertexSource {
                        view: view.clone(),
                        cx: &cx,
                        as_of: basis,
                        after: None,
                    },
                    plan.clone(),
                    wide,
                    || {
                        let n = calls.get() + 1;
                        calls.set(n);
                        if n == stop { Err(stop) } else { Ok(()) }
                    },
                );
                assert_eq!(Arc::strong_count(&view.snapshot), refs + 1);
                let mut delivered = Vec::new();
                loop {
                    match cursor.next() {
                        Some(Ok(row)) => delivered.push(row),
                        Some(Err(GqlQueryError::Interrupted(at))) => {
                            assert_eq!(at, stop);
                            break;
                        }
                        other => panic!("interruption became {other:?}"),
                    }
                }
                assert_eq!(delivered, expected[..delivered.len()]);
                assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
                assert_eq!(cursor.state(), VertexScanState::Failed);
                assert_eq!(Arc::strong_count(&view.snapshot), refs);
                assert!(cursor.next().is_none());
                cursor.close();
                assert_eq!(calls.get(), stop);
                let mut retry = VertexScanCursor::new(
                    SnapshotVertexSource {
                        view: view.clone(),
                        cx: &cx,
                        as_of: basis,
                        after: None,
                    },
                    plan.clone(),
                    wide,
                    || Ok::<_, usize>(()),
                );
                assert_eq!(
                    retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                    expected
                );
                assert_eq!(retry.evaluator_stats(), stats);
                assert_eq!(db.frontier().unwrap(), basis);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
