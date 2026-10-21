//! Map projections reuse the metadata fixture and the public transaction path.

use super::*;

fn value_map(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(entries.into_iter().map(|(key, value)| (key.into(), value)).collect())
        .unwrap()
}

#[test]
fn row_map_wildcards_selectors_and_overrides_return_final_values_after_reopen() {
    run(async |commit, cx, txcx| {
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(commit, vfs.clone(), &path, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(
            &mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH 7 AS pad,n AS source \
             SET source.p=source.p+10,source.q=source.p*10 \
             RETURN source.p AS p,source{.*,pad,p:-1,doubled:source.q*2} AS document, \
             source{.p,.q} AS selected ORDER BY p LIMIT 1", 1,
        ).await.unwrap();
        assert_eq!(rows(result, &["p", "document", "selected"]), vec![vec![
            int(11), map(&[("doubled", 20), ("p", -1), ("pad", 7), ("q", 10)]),
            map(&[("p", 11), ("q", 10)]),
        ]]);
        // Neither result paging nor the map's p override changes the effects.
        assert_eq!(read_p(&db, cx), vec![vec![int(11)], vec![int(12)]]);
        db.compact(commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(commit, vfs, &path, keys()).await.unwrap();
        let result = db.query(
            cx, "MATCH (n:Item) RETURN n.p AS p,n.q AS q ORDER BY p",
            &GqlParameters::new(), Catalog, query_policy(2),
        ).unwrap();
        assert_eq!(rows(result, &["p", "q"]), vec![vec![int(11), int(10)], vec![int(12), int(20)]]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn nullable_row_map_sources_guard_empty_literal_selected_and_wildcard_maps() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = read_p(&db, cx);
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (a:Item) OPTIONAL MATCH (b:Missing) WITH a,b SET b.p=99 \
             RETURN a.p AS p,b{} AS empty,b{value:1} AS literal,b{.p} AS selected, \
             b{.*} AS wildcard ORDER BY p", 2,
        ).await.unwrap();
        assert_eq!(rows(result, &["p", "empty", "literal", "selected", "wildcard"]), vec![
            vec![int(1), null(), null(), null(), null()],
            vec![int(2), null(), null(), null(), null()],
        ]);
        assert_eq!(read_p(&db, cx), before);
    });
}

#[test]
fn row_relationship_map_selectors_and_wildcards_distinguish_removed_properties() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (a)-[r:R]->(b) WITH r AS edge SET edge.p=edge.p+10 \
             RETURN edge{.p} AS selected,edge{.*} AS wildcard", 1,
        ).await.unwrap();
        assert_eq!(rows(result, &["selected", "wildcard"]),
            vec![vec![map(&[("p", 17)]), map(&[("p", 17)])]]);
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (a)-[r:R]->(b) WITH r AS edge REMOVE edge.p \
             RETURN edge{.p} AS selected,edge{.*} AS wildcard", 1,
        ).await.unwrap();
        assert_eq!(rows(result, &["selected", "wildcard"]),
            vec![vec![value_map(vec![("p", null())]), map(&[])]]);
    });
}

#[test]
fn row_map_aggregate_arguments_preserve_occurrences_and_feed_a_native_tail() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (a:Item),(b:Item) WITH a AS source SET source.p=source.p+10 \
             RETURN size(collect(source{.p})) AS occurrences,count(*)+1 AS total, \
             size(collect(DISTINCT source{.*})) AS documents", 1,
        ).await.unwrap();
        assert_eq!(rows(result, &["occurrences", "total", "documents"]),
            vec![vec![int(4), int(5), int(2)]]);
        assert_eq!(read_p(&db, cx), vec![vec![int(11)], vec![int(12)]]);
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
             WITH source{.p} AS document WHERE document.p>21 \
             RETURN document,document.p AS p", 1,
        ).await.unwrap();
        assert_eq!(rows(result, &["document", "p"]),
            vec![vec![map(&[("p", 22)]), int(22)]]);
        assert_eq!(read_p(&db, cx), vec![vec![int(21)], vec![int(22)]]);
    });
}

#[test]
fn late_row_map_failure_restores_only_its_statement_and_limit_zero_still_evaluates() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(BASE+99), vec![], vec![(P, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let statement = "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
                         RETURN source{.*,bad:12/(source.p-$denominator)} AS document LIMIT 0";
        let failure = txn.query_write(
            &mut db, cx, statement, &GqlParameters::new().with_int64("denominator", 12).unwrap(),
            Catalog, R, policy(0), no_allocation,
        );
        assert!(matches!(failure, Err(QueryWriteError::Mutation(GqlQueryError::Source(_)))));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
        // Same statement and output limit with valid data must actually run.
        let result = txn.query_write(
            &mut db, cx, statement, &GqlParameters::new().with_int64("denominator", 100).unwrap(),
            Catalog, R, policy(0), no_allocation,
        ).unwrap();
        assert!(rows(result, &["document"]).is_empty());
        txn.commit(&mut db, commit).await.unwrap();
        assert_eq!(read_p(&db, cx), vec![vec![int(11)], vec![int(12)]]);
        assert!(db.vertex(VId(BASE+99)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn row_map_scopes_do_not_revive_hidden_dropped_or_wrongly_typed_sources() {
    for statement in [
        "MATCH (n) WITH n AS kept SET kept.p=1 RETURN n{.*}",
        "MATCH (n) WITH n SET n.p=1 RETURN __fg_boundary_0{.*}",
        "MATCH (n) WITH n SET n.p=1 RETURN n{__fg_boundary_0}",
        "MATCH (n) WITH n,7 AS scalar SET n.p=1 RETURN scalar{.*}",
        "MATCH (a)-[r:R]->(b) WITH a,r DETACH DELETE a RETURN r{.*}",
        "MATCH (a)-[r:R]->(b) WITH a,r DETACH DELETE a RETURN r{.p}",
        "MATCH (a)-[r:R]->(b) WITH a,r DETACH DELETE a RETURN r.p",
    ] {
        let mut calls = 0;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            PreparedGraphMutationQueryText::prepare(statement, R, |kind, name: &str| {
                calls += 1;
                Catalog.resolve_symbol(kind, name)
            })
        }));
        assert!(result.is_ok(), "preparation panicked: {statement}");
        assert!(result.unwrap().is_err(), "{statement}");
        assert_eq!(calls, 0, "{statement}");
    }
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH n AS __fg_boundary_0,7 AS scalar \
             SET __fg_boundary_0.p=__fg_boundary_0.p+10 \
             RETURN __fg_boundary_0.p AS p,__fg_boundary_0{scalar} AS document ORDER BY p", 2,
        ).await.unwrap();
        assert_eq!(rows(result, &["p", "document"]), vec![
            vec![int(11), map(&[("scalar", 7)])], vec![int(12), map(&[("scalar", 7)])],
        ]);
    });
}

#[test]
fn row_map_output_budget_failure_and_deleted_map_reads_do_not_publish_effects() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.frontier().unwrap();
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
             RETURN source{.*} AS document", 1,
        ).await;
        assert!(matches!(result, Err(QueryWriteError::Mutation(GqlQueryError::Rows(_)))));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH n AS source DETACH DELETE source \
             RETURN source{.*} AS document LIMIT 0", 0,
        ).await;
        assert!(matches!(result, Err(QueryWriteError::Mutation(GqlQueryError::Source(_)))));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH n AS source DETACH DELETE source RETURN 1 AS done LIMIT 0", 0,
        ).await.unwrap();
        assert!(rows(result, &["done"]).is_empty());
        assert!(read_p(&db, cx).is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}
