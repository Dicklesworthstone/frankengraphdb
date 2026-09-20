use super::*;
use asupersync::lab::run_async_under_lab;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use fgdb_gql::{GqlParameters, PreparedGraphSetText, PreparedGraphText};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000) }
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x61; 32], DatabaseSecurityNamespaceId([0x62; 32]), [0x63; 32]) }
fn none(_: fgdb_gql::GraphSymbolKind, _: &str) -> Option<fgdb_gql::GraphSymbol> { None }
fn bound() -> PreparedGraphSet {
    PreparedGraphSetText::prepare("(MATCH (n) RETURN n AS id UNION ALL MATCH (n) RETURN n AS id) EXCEPT ALL MATCH (n) RETURN n AS id", none)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn addresses(db: &Database<MemVfs>, first: usize, root: usize) -> Vec<usize> {
    db.standing_queries[first..=root].iter().map(|query| match query {
        StandingQuery::Rows { source, .. } => source.as_ref() as *const _ as usize,
        StandingQuery::Set(query) => query.as_ref() as *const _ as usize,
        _ => panic!("not a native set node"),
    }).collect()
}

#[test]
fn every_circuit_stage_refusal_and_unwind_drops_only_the_unpublished_suffix() {
    let ((), report) = run_async_under_lab(0x6e73_7210, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1)); seed.create_vertex(VId(1), vec![], vec![]);
        db.write(&commit, seed).await.unwrap();
        let prior = db.register_standing_native(&cx, "MATCH (n) RETURN n AS id", &GqlParameters::new(), none, policy()).unwrap();
        let before = db.standing_queries.len();
        let answer = db.standing_native_query(&cx, &prior, policy()).unwrap();
        let query = bound(); let mut calls = 0;
        {
            let mut staged = Staging::new(&mut db);
            staged.compile(&cx, &query, policy(), &mut || { calls += 1; Ok(()) }).unwrap();
            assert_eq!(staged.database.standing_queries.len(), before + 5);
            // Deliberate downstream drop even after every node prepared.
        }
        assert_eq!(db.standing_queries.len(), before);
        for stop in 1..=calls {
            let mut visited = 0;
            {
                let mut staged = Staging::new(&mut db);
                assert!(staged.compile(&cx, &query, policy(), &mut || {
                    visited += 1;
                    if visited == stop { Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted)) }
                    else { Ok(()) }
                }).is_err());
            }
            assert_eq!(visited, stop);
            assert_eq!(db.standing_queries.len(), before);
            assert_eq!(db.standing_native_query(&cx, &prior, policy()).unwrap(), answer);
        }
        let leaf = PreparedGraphSet::from(PreparedGraphText::prepare("MATCH (n) RETURN n AS id", none)
            .unwrap().bind_parameters(&GqlParameters::new()).unwrap());
        let invalid = leaf.clone().combine(GraphSetOperation::Union, GraphSetQuantifier::All,
            leaf.with_page(0, Some(0))).unwrap();
        assert!(register(&mut db, &cx, &invalid, policy()).is_err());
        assert_eq!(db.standing_queries.len(), before, "valid left operand must be reclaimed");
        // A panic before acceptance uses the same Drop guard, not a special
        // cancellation mode. No expected panic escapes the lab supervisor.
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut staged = Staging::new(&mut db);
            staged.compile(&cx, &query, policy(), &mut || Ok(())).unwrap();
            panic!("injected downstream unwind");
        }));
        assert!(panic.is_err());
        assert_eq!(db.standing_queries.len(), before);
        assert_eq!(db.standing_native_query(&cx, &prior, policy()).unwrap(), answer);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn complete_native_rebuild_replaces_no_old_node_until_every_stage_is_accepted() {
    let ((), report) = run_async_under_lab(0x6e73_7211, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1)); seed.create_vertex(VId(1), vec![], vec![]);
        db.write(&commit, seed).await.unwrap();
        let handle = register(&mut db, &cx, &bound(), policy()).unwrap();
        let Layout::Circuit { first, .. } = handle.native.as_deref().unwrap() else { panic!("circuit layout"); };
        let first = *first; let root = handle.index;
        let size = db.standing_queries.len(); let before = db.standing_native_query(&cx, &handle, policy()).unwrap();
        let mut calls = 0;
        rebuild_checked(&mut db, &cx, first, root, policy(), &mut || { calls += 1; Ok(()) }).unwrap();
        assert!(calls > 0);
        for stop in 1..=calls {
            let identity = addresses(&db, first, root); let mut visited = 0;
            assert!(rebuild_checked(&mut db, &cx, first, root, policy(), &mut || {
                visited += 1;
                if visited == stop { Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted)) }
                else { Ok(()) }
            }).is_err());
            assert_eq!(visited, stop);
            assert_eq!(addresses(&db, first, root), identity);
            assert_eq!(db.standing_queries.len(), size);
            assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap(), before);
        }
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert_eq!(db.standing_queries.len(), size);
        let mut next = WriteBatch::new(RelationId(1)); next.create_vertex(VId(2), vec![], vec![]);
        db.write(&commit, next).await.unwrap();
        let (_, QueryResult::Rows { rows, .. }) = db.standing_native_query(&cx, &handle, policy()).unwrap()
            else { panic!("native rows"); };
        assert_eq!(rows.len(), 2, "rebased dependencies must read original public indexes");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
