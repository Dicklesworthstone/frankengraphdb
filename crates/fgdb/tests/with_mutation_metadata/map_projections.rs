//! Map projections reuse the metadata fixture and the public transaction path.

use super::*;

fn value_map(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
    .unwrap()
}

#[test]
fn row_map_wildcards_selectors_and_overrides_return_final_values_after_reopen() {
    run(async |commit, cx, txcx| {
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH 7 AS pad,n AS source \
             SET source.p=source.p+10,source.q=source.p*10 \
             RETURN source.p AS p,source{.*,pad,p:-1,doubled:source.q*2} AS document, \
             source{.p,.q} AS selected ORDER BY p LIMIT 1",
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["p", "document", "selected"]),
            vec![vec![
                int(11),
                map(&[("doubled", 20), ("p", -1), ("pad", 7), ("q", 10)]),
                map(&[("p", 11), ("q", 10)]),
            ]]
        );
        // Neither result paging nor the map's p override changes the effects.
        assert_eq!(read_p(&db, cx), vec![vec![int(11)], vec![int(12)]]);
        db.compact(commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(commit, vfs, &path, keys())
            .await
            .unwrap();
        let result = db
            .query(
                cx,
                "MATCH (n:Item) RETURN n.p AS p,n.q AS q ORDER BY p",
                &GqlParameters::new(),
                Catalog,
                query_policy(2),
            )
            .unwrap();
        assert_eq!(
            rows(result, &["p", "q"]),
            vec![vec![int(11), int(10)], vec![int(12), int(20)]]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn nullable_row_map_sources_guard_empty_literal_selected_and_wildcard_maps() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = read_p(&db, cx);
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (a:Item) OPTIONAL MATCH (b:Missing) WITH a,b SET b.p=99 \
             RETURN a.p AS p,b{} AS empty,b{value:1} AS literal,b{.p} AS selected, \
             b{.*} AS wildcard ORDER BY p",
            2,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["p", "empty", "literal", "selected", "wildcard"]),
            vec![
                vec![int(1), null(), null(), null(), null()],
                vec![int(2), null(), null(), null(), null()],
            ]
        );
        assert_eq!(read_p(&db, cx), before);
    });
}

#[test]
fn row_relationship_map_selectors_and_wildcards_distinguish_removed_properties() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (a)-[r:R]->(b) WITH r AS edge SET edge.p=edge.p+10 \
             RETURN edge{.p} AS selected,edge{.*} AS wildcard",
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["selected", "wildcard"]),
            vec![vec![map(&[("p", 17)]), map(&[("p", 17)])]]
        );
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (a)-[r:R]->(b) WITH r AS edge REMOVE edge.p \
             RETURN edge{.p} AS selected,edge{.*} AS wildcard",
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["selected", "wildcard"]),
            vec![vec![value_map(vec![("p", null())]), map(&[])]]
        );
    });
}

#[test]
fn row_map_aggregate_arguments_preserve_occurrences_and_feed_a_native_tail() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (a:Item),(b:Item) WITH a AS source SET source.p=source.p+10 \
             RETURN size(collect(source{.p})) AS occurrences,count(*)+1 AS total, \
             size(collect(DISTINCT source{.*})) AS documents",
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["occurrences", "total", "documents"]),
            vec![vec![int(4), int(5), int(2)]]
        );
        assert_eq!(read_p(&db, cx), vec![vec![int(11)], vec![int(12)]]);
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
             WITH source{.p} AS document WHERE document.p>21 \
             RETURN document,document.p AS p",
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["document", "p"]),
            vec![vec![map(&[("p", 22)]), int(22)]]
        );
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
        prefix.create_vertex(VId(BASE + 99), vec![], vec![(P, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let statement = "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
                         RETURN source{.*,bad:12/(source.p-$denominator)} AS document LIMIT 0";
        let failure = txn.query_write(
            &mut db,
            cx,
            statement,
            &GqlParameters::new().with_int64("denominator", 12).unwrap(),
            Catalog,
            R,
            policy(0),
            no_allocation,
        );
        assert!(matches!(
            failure,
            Err(QueryWriteError::Mutation(GqlQueryError::Source(_)))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
        // Same statement and output limit with valid data must actually run.
        let result = txn
            .query_write(
                &mut db,
                cx,
                statement,
                &GqlParameters::new().with_int64("denominator", 100).unwrap(),
                Catalog,
                R,
                policy(0),
                no_allocation,
            )
            .unwrap();
        assert!(rows(result, &["document"]).is_empty());
        txn.commit(&mut db, commit).await.unwrap();
        assert_eq!(read_p(&db, cx), vec![vec![int(11)], vec![int(12)]]);
        assert!(db.vertex(VId(BASE + 99)).unwrap().is_some());
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
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n AS __fg_boundary_0,7 AS scalar \
             SET __fg_boundary_0.p=__fg_boundary_0.p+10 \
             RETURN __fg_boundary_0.p AS p,__fg_boundary_0{scalar} AS document ORDER BY p",
            2,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["p", "document"]),
            vec![
                vec![int(11), map(&[("scalar", 7)])],
                vec![int(12), map(&[("scalar", 7)])],
            ]
        );
    });
}

#[test]
fn row_map_output_budget_failure_and_deleted_map_reads_do_not_publish_effects() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.frontier().unwrap();
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
             RETURN source{.*} AS document",
            1,
        )
        .await;
        assert!(matches!(
            result,
            Err(QueryWriteError::Mutation(GqlQueryError::Rows(_)))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n AS source DETACH DELETE source \
             RETURN source{.*} AS document LIMIT 0",
            0,
        )
        .await;
        assert!(matches!(
            result,
            Err(QueryWriteError::Mutation(GqlQueryError::Source(_)))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n AS source DETACH DELETE source RETURN 1 AS done LIMIT 0",
            0,
        )
        .await
        .unwrap();
        assert!(rows(result, &["done"]).is_empty());
        assert!(read_p(&db, cx).is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

// Computed DELETE operands use a different parsed action than bare names.
// These laws exercise real preparation and transaction execution so that a
// blanket refusal of computed deletion cannot masquerade as the cascade fix.
#[test]
fn computed_detach_targets_refuse_relationship_reads_before_catalog_access() {
    use fgdb_gql::{GraphMutationTextErrorKind, GraphPatternTextErrorKind};

    for target in [
        "a",
        "[a][0]",
        "head([a])",
        "CASE WHEN true THEN a ELSE b END",
        "[a,b][$which]",
    ] {
        let prefix = format!("MATCH (a)-[r:R]->(b) WITH a,b,r DETACH DELETE {target}");
        let control = format!("{prefix} RETURN 1 AS done");
        let arguments = if target.contains("$which") {
            GqlParameters::new().with_int64("which", 0).unwrap()
        } else {
            GqlParameters::new()
        };
        PreparedGraphMutationQueryText::prepare(&control, R, Catalog)
            .unwrap_or_else(|error| panic!("valid target refused: {control}: {error:?}"))
            .bind_parameters(&arguments)
            .unwrap_or_else(|error| panic!("valid target did not bind: {control}: {error:?}"));

        for tail in [
            "RETURN r.p AS value",
            "RETURN properties(r) AS value",
            "RETURN keys(r) AS value",
            "RETURN r{.p} AS value",
            "RETURN r{.*} AS value",
            "RETURN count(r.p) AS value",
            "WITH r AS edge RETURN edge.p AS value",
            "RETURN r.p AS value LIMIT 0",
            "RETURN CASE WHEN false THEN r.p ELSE 0 END AS value",
        ] {
            let statement = format!("{prefix} {tail}");
            let mut calls = 0;
            let result = PreparedGraphMutationQueryText::prepare(
                &statement,
                R,
                |kind, name: &str| {
                    calls += 1;
                    Catalog.resolve_symbol(kind, name)
                },
            );
            let Err(error) = result else {
                panic!("post-cascade relationship read admitted: {statement}");
            };
            assert_eq!(
                error.kind,
                GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::Expected(
                    "a vertex property after DETACH DELETE, which may cascade to edges"
                )),
                "refusal must identify the cascade, not an unrelated syntax error: {statement}"
            );
            assert_eq!(calls, 0, "catalog called for refused output: {statement}");
        }
    }
}

#[test]
fn path_alias_and_mixed_computed_detach_targets_keep_the_cascade_guard() {
    use fgdb_gql::{GraphMutationTextErrorKind, GraphPatternTextErrorKind};

    for prefix in [
        "MATCH (a)-[r:R]->(b) WITH a AS chosen,r AS edge DETACH DELETE [chosen][0]",
        "MATCH route=(a)-[r:R]->(b) WITH route,r AS edge DETACH DELETE route",
        "MATCH (a)-[r:R]->(b) WITH a,r AS edge DETACH DELETE null,[a][0]",
        "MATCH (a)-[r:R]->(b) WITH a,r AS edge DETACH DELETE [a][0],null",
    ] {
        let control = format!("{prefix} RETURN 1 AS done");
        PreparedGraphMutationQueryText::prepare(&control, R, Catalog)
            .unwrap_or_else(|error| panic!("valid control refused: {control}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        for output in ["edge.p", "edge{.*}"] {
            let statement = format!("{prefix} RETURN {output} AS value");
            let mut calls = 0;
            let result = PreparedGraphMutationQueryText::prepare(
                &statement,
                R,
                |kind, name: &str| {
                    calls += 1;
                    Catalog.resolve_symbol(kind, name)
                },
            );
            let Err(error) = result else {
                panic!("computed cascade escaped: {statement}");
            };
            assert_eq!(
                error.kind,
                GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::Expected(
                    "a vertex property after DETACH DELETE, which may cascade to edges"
                )),
                "{statement}"
            );
            assert_eq!(calls, 0, "{statement}");
        }
    }
}

#[test]
fn computed_detach_keeps_explicit_prewrite_values_and_persists_its_cascade() {
    for limited in [false, true] {
        run(async |commit, cx, txcx| {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            db.write(commit, fixture()).await.unwrap();
            let survivor = db.vertex(VId(BASE + 2)).unwrap();
            let statement = if limited {
                "MATCH (a)-[r:R]->(b) WITH a,r.p AS prior \
                 DETACH DELETE [a][0] RETURN prior LIMIT 0"
            } else {
                "MATCH (a)-[r:R]->(b) WITH a,r.p AS prior \
                 DETACH DELETE [a][0] RETURN prior"
            };
            let result = apply(&mut db, commit, cx, txcx, statement, u64::from(!limited))
                .await
                .unwrap();
            let expected = if limited {
                Vec::new()
            } else {
                vec![vec![int(7)]]
            };
            assert_eq!(rows(result, &["prior"]), expected);
            assert!(db.vertex(VId(BASE + 1)).unwrap().is_none());
            assert!(db.edge(EId(BASE + 10)).unwrap().is_none());
            assert_eq!(db.vertex(VId(BASE + 2)).unwrap(), survivor);
            let frontier = db.frontier().unwrap();
            db.compact(commit).await.unwrap();
            drop(db);
            let db = Database::open_with_vfs(commit, vfs, &path, keys()).await.unwrap();
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(VId(BASE + 1)).unwrap().is_none());
            assert!(db.edge(EId(BASE + 10)).unwrap().is_none());
            assert_eq!(db.vertex(VId(BASE + 2)).unwrap(), survivor);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
    }
}

#[test]
fn refused_computed_cascade_read_preserves_prior_staging_and_can_be_retried_safely() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let mut txn = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(BASE + 99), vec![], vec![(P, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let refused = "MATCH (a)-[r:R]->(b) WITH a,r \
                       DETACH DELETE [a][0] RETURN r.p AS stale LIMIT 0";
        assert!(
            txn.query_write(
                &mut db,
                cx,
                refused,
                &GqlParameters::new(),
                Catalog,
                R,
                policy(0),
                no_allocation,
            )
            .is_err()
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );

        let control = "MATCH (a)-[r:R]->(b) WITH a,r \
                       DETACH DELETE [a][0] RETURN 1 AS done LIMIT 0";
        let result = txn
            .query_write(
                &mut db,
                cx,
                control,
                &GqlParameters::new(),
                Catalog,
                R,
                policy(0),
                no_allocation,
            )
            .unwrap();
        assert!(rows(result, &["done"]).is_empty());
        txn.commit(&mut db, commit).await.unwrap();
        assert!(db.vertex(VId(BASE + 99)).unwrap().is_some());
        assert!(db.vertex(VId(BASE + 1)).unwrap().is_none());
        assert!(db.vertex(VId(BASE + 2)).unwrap().is_some());
        assert!(db.edge(EId(BASE + 10)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn plain_computed_delete_can_return_an_unaffected_relationships_properties() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let mut isolated = WriteBatch::new(R);
        isolated.create_vertex(VId(BASE + 99), vec![MISSING], vec![]);
        db.write(commit, isolated).await.unwrap();
        let edge = db.edge(EId(BASE + 10)).unwrap();
        let endpoints = (
            db.vertex(VId(BASE + 1)).unwrap(),
            db.vertex(VId(BASE + 2)).unwrap(),
        );
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (a)-[r:R]->(b),(isolated:Missing) WITH isolated,r \
             DELETE head([isolated]) RETURN r.p AS value,r{.*} AS document",
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["value", "document"]),
            vec![vec![int(7), map(&[("p", 7)])]]
        );
        assert!(db.vertex(VId(BASE + 99)).unwrap().is_none());
        assert_eq!(db.edge(EId(BASE + 10)).unwrap(), edge);
        assert_eq!(
            (
                db.vertex(VId(BASE + 1)).unwrap(),
                db.vertex(VId(BASE + 2)).unwrap()
            ),
            endpoints
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn explicitly_deleted_computed_targets_keep_runtime_metadata_checks() {
    for (action, output, detaches) in [
        ("DETACH DELETE [a][0]", "properties(a)", true),
        ("DELETE head([r])", "properties(r)", false),
    ] {
        run(async |commit, cx, txcx| {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            db.write(commit, fixture()).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let statement = format!(
                "MATCH (a)-[r:R]->(b) WITH a,r {action} RETURN {output} AS value LIMIT 0"
            );
            // The compiler must not confuse a plain DELETE with a cascade or
            // reject all metadata merely because the target is computed.
            PreparedGraphMutationQueryText::prepare(&statement, R, Catalog)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let result = apply(&mut db, commit, cx, txcx, &statement, 0).await;
            assert!(matches!(
                result,
                Err(QueryWriteError::Mutation(GqlQueryError::Source(_)))
            ));
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );

            let control = format!(
                "MATCH (a)-[r:R]->(b) WITH a,r {action} RETURN 1 AS done LIMIT 0"
            );
            let result = apply(&mut db, commit, cx, txcx, &control, 0)
                .await
                .unwrap();
            assert!(rows(result, &["done"]).is_empty());
            assert_eq!(db.vertex(VId(BASE + 1)).unwrap().is_none(), detaches);
            assert!(db.vertex(VId(BASE + 2)).unwrap().is_some());
            assert!(db.edge(EId(BASE + 10)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
    }
}
