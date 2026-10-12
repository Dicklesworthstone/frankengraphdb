//! CREATE/INSERT map returns through the public FGP write/result boundary.
use super::*;

fn map(entries: Vec<(&str, WireValue)>) -> WireValue {
    WireValue::Map(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

#[test]
fn insertion_returning_maps_use_catalog_names_for_nodes_edges_and_wildcards() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "insertion-returning-property-maps").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        for (index, verb) in ["CREATE", "INSERT"].into_iter().enumerate() {
            let statement = format!(
                "{verb} (a:Person {{name:'Ann',age:30}}),(b:Person),\
                 (a)-[r:KNOWS {{name:'friend',age:7}}]->(b) \
                 RETURN properties(a) AS props, keys(a) AS names, \
                 properties(r) AS edge_props, keys(r) AS edge_names, \
                 a{{.*,name:'view'}} AS projected, r{{.*,age:8}} AS edge_projected, \
                 b{{.*}} AS empty"
            );
            let created = owner
                .execute(cx, ExecuteMode::Write, &statement, vec![])
                .await
                .unwrap();
            assert_eq!(
                created.columns,
                [
                    "props",
                    "names",
                    "edge_props",
                    "edge_names",
                    "projected",
                    "edge_projected",
                    "empty"
                ]
            );
            // Property IDs are name=1 and age=2, deliberately the reverse of
            // canonical name order. Neither field may become a numeric key.
            assert_eq!(
                created.rows,
                [vec![
                    map(vec![("age", WireValue::Int(30)), ("name", text("Ann"))]),
                    WireValue::List(vec![text("age"), text("name")]),
                    map(vec![("age", WireValue::Int(7)), ("name", text("friend"))]),
                    WireValue::List(vec![text("age"), text("name")]),
                    map(vec![("age", WireValue::Int(30)), ("name", text("view"))]),
                    map(vec![("age", WireValue::Int(8)), ("name", text("friend"))]),
                    map(vec![]),
                ]]
            );
            assert_eq!(
                created.outcome,
                Outcome::WriteCommitted {
                    seq: index as u64 + 1,
                    statements: 1
                }
            );
        }
        let stored = owner
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (a)-[r:KNOWS]->(b) RETURN properties(a) AS props, \
                 properties(r) AS edge_props, properties(b) AS empty",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            stored.rows,
            vec![
                vec![
                    map(vec![("age", WireValue::Int(30)), ("name", text("Ann"))]),
                    map(vec![("age", WireValue::Int(7)), ("name", text("friend"))]),
                    map(vec![]),
                ];
                2
            ]
        );
        assert_eq!(stored.outcome, Outcome::Rows { seq: 2 });
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn insertion_returning_maps_mask_matched_sources_and_refuse_late_failures_atomically() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "scoped-insertion-returning-maps").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (a:Person {name:'Ann',age:30}),(b:Person),\
                 (a)-[:KNOWS {name:'private',age:5}]->(b)",
                vec![],
            )
            .await
            .unwrap();
        let mut scope = grant(Rights::ReadWrite);
        scope.labels = Scope::only([LabelId(1)]);
        scope.relations = Scope::only([RelationId(1)]);
        scope.properties = Scope::only([PropertyKeyId(2)]);
        let mut limited = Client::connect(cx, addr, token(&scope)).await.unwrap();
        limited.select(cx, "social").await.unwrap();
        let created = limited
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (a:Person)-[old:KNOWS]->(b:Person) WHERE a.age=30 \
                 CREATE (c:Person {age:a.age+1})-[fresh:KNOWS {age:old.age+1}]->(a) \
                 RETURN properties(a) AS source, keys(a) AS names, old{.*} AS old_props, \
                 properties(c) AS created, fresh{.*} AS fresh_props",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            created.rows,
            [vec![
                map(vec![("age", WireValue::Int(30))]),
                WireValue::List(vec![text("age")]),
                map(vec![("age", WireValue::Int(5))]),
                map(vec![("age", WireValue::Int(31))]),
                map(vec![("age", WireValue::Int(6))]),
            ]]
        );
        assert_eq!(
            created.outcome,
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1
            }
        );
        for verb in ["CREATE", "INSERT"] {
            for suffix in ["", " LIMIT 0"] {
                let statement = format!(
                    "UNWIND [2,0] AS x {verb} (n:Person {{age:x}})-[r:KNOWS {{age:x}}]->(n) \
                     RETURN properties(n) AS props, properties(r) AS edge_props, \
                     n{{.*,ratio:10/x}} AS projected{suffix}"
                );
                let mut columns = 0;
                let mut rows = 0;
                let failure = limited
                    .execute_streaming(
                        cx,
                        ExecuteMode::Write,
                        &statement,
                        vec![],
                        |_| {
                            columns += 1;
                            Ok(())
                        },
                        |_| {
                            rows += 1;
                            Ok(())
                        },
                    )
                    .await
                    .unwrap_err();
                assert_eq!(server_code(failure), ErrorCode::Statement);
                assert_eq!((columns, rows), (0, 0));
                let current = owner
                    .execute(
                        cx,
                        ExecuteMode::Read,
                        "MATCH (n:Person) RETURN count(n) AS count",
                        vec![],
                    )
                    .await
                    .unwrap();
                assert_eq!(current.rows, [[WireValue::Count(3)]]);
                assert_eq!(current.outcome, Outcome::Rows { seq: 2 });
            }
        }
        let edges = owner
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (a)-[r:KNOWS]->(b) RETURN count(r) AS count",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(edges.rows, [[WireValue::Count(2)]]);
        assert_eq!(edges.outcome, Outcome::Rows { seq: 2 });
        let valid = limited
            .execute(
                cx,
                ExecuteMode::Write,
                "INSERT (n:Person {age:99}) RETURN n{.*} AS props",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(valid.rows, [[map(vec![("age", WireValue::Int(99))])]]);
        assert_eq!(
            valid.outcome,
            Outcome::WriteCommitted {
                seq: 3,
                statements: 1
            }
        );
        limited.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}
