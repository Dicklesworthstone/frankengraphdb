//! Correlated graph predicates after row stages use the carried identities,
//! preserve outer occurrences, and retain capability masking over real FGP.

use super::*;

async fn connect(cx: &Cx, address: SocketAddr, scope: &Grant) -> Client {
    let mut client = Client::connect(cx, address, token(scope)).await.unwrap();
    client.select(cx, "social").await.unwrap();
    client
}

async fn seed(cx: &Cx, owner: &mut Client) {
    owner
        .execute(
            cx,
            ExecuteMode::Write,
            "CREATE (a:Person {name:'Ann',age:10}), \
             (b:Person {name:'Bob',age:20}), (c:Person {name:'Cy',age:30}), \
             (d:Person {name:'Dana',age:40}), (e:Person {name:'Eve',age:50}), \
             (hidden:Company {name:'Hidden',age:99}), \
             (a)-[:KNOWS]->(b), (a)-[:KNOWS]->(c), (b)-[:KNOWS]->(c), \
             (c)-[:WORKS_AT]->(a), (d)-[:KNOWS]->(hidden)",
            vec![],
        )
        .await
        .unwrap();
}

fn floor(value: i64) -> Vec<(String, WireValue)> {
    vec![("floor".into(), WireValue::Int(value))]
}

fn names(values: &[&str]) -> Vec<Vec<WireValue>> {
    values.iter().map(|value| vec![text(value)]).collect()
}

#[test]
fn row_predicates_preserve_collected_identities_outer_bags_and_prepared_rebinding() {
    run(async |cx| {
        let (address, shutdown, mut task) = start(cx, "row-predicate-bags").await;
        let mut owner = connect(cx, address, &grant(Rights::ReadWrite)).await;
        seed(cx, &mut owner).await;
        let mut reader = connect(cx, address, &grant(Rights::Read)).await;
        let statement = "MATCH (n:Person) WHERE n.age >= $floor \
                         WITH collect(n) AS people UNWIND people AS person \
                         WHERE EXISTS((person)-[:KNOWS]->()) \
                         RETURN person.name AS name ORDER BY name";
        let prepared = reader.prepare_read(cx, statement, floor(99)).await.unwrap();
        for (minimum, expected) in [
            (0, &["Ann", "Bob", "Dana"][..]),
            (15, &["Bob", "Dana"][..]),
            (45, &[][..]),
        ] {
            let direct = reader
                .execute(cx, ExecuteMode::Read, statement, floor(minimum))
                .await
                .unwrap();
            assert_eq!(direct.columns, ["name"]);
            assert_eq!(direct.rows, names(expected));
            assert_eq!(direct.outcome, Outcome::Rows { seq: 1 });
            assert_eq!(
                reader
                    .execute_prepared(cx, prepared, floor(minimum))
                    .await
                    .unwrap(),
                direct,
            );
        }

        // Ann has two witnesses, but existence retains one result for each
        // outer occurrence. Duplicating the collected identities deliberately
        // yields six rows, crossing the server's four-row flow window.
        let duplicated = "MATCH (n:Person) WITH n ORDER BY n.age DESC \
                          WITH collect(n) AS people UNWIND people AS candidate \
                          UNWIND [candidate,candidate] AS person \
                          WHERE (person)-[:KNOWS]->()";
        let all = reader
            .execute(
                cx,
                ExecuteMode::Read,
                &format!("{duplicated} RETURN person.age AS age ORDER BY age DESC"),
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(all.columns, ["age"]);
        assert_eq!(
            all.rows,
            [40, 40, 20, 20, 10, 10].map(|age| vec![WireValue::Int(age)])
        );
        let paged = reader
            .execute(
                cx,
                ExecuteMode::Read,
                &format!("{duplicated} LIMIT 3 RETURN person.age AS age ORDER BY age DESC"),
                vec![],
            )
            .await
            .unwrap();
        // The predicate's internal projection must retain the incoming order
        // until this page. Reordering by identity or deduplicating outer rows
        // before LIMIT would select a different bag.
        assert_eq!(
            paged.rows,
            [40, 40, 20].map(|age| vec![WireValue::Int(age)])
        );

        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (a:Person {name:'Eve'}), (b:Person {name:'Ann'}) \
                 CREATE (a)-[:KNOWS]->(b)",
                vec![],
            )
            .await
            .unwrap();
        let current = reader
            .execute_prepared(cx, prepared, floor(45))
            .await
            .unwrap();
        assert_eq!(current.rows, names(&["Eve"]));
        assert_eq!(current.outcome, Outcome::Rows { seq: 2 });

        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn row_predicates_mask_relations_and_target_vertices_before_positive_or_negative_existence() {
    run(async |cx| {
        let (address, shutdown, mut task) = start(cx, "row-predicate-masks").await;
        let mut owner = connect(cx, address, &grant(Rights::ReadWrite)).await;
        seed(cx, &mut owner).await;
        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        scope.relations = Scope::only([RelationId(1)]);
        let mut masked = connect(cx, address, &scope).await;

        // Dana's only KNOWS target is a hidden Company. Cy's only outgoing
        // relation is hidden WORKS_AT. Neither may become a positive witness
        // or suppress an outer row in the negated forms.
        let cases: &[(&str, &[&str], &[&str])] = &[
            (
                "(person)-[:KNOWS]->()",
                &["Ann", "Bob", "Dana"],
                &["Ann", "Bob"],
            ),
            (
                "NOT EXISTS((person)-[:KNOWS]->())",
                &["Cy", "Eve"],
                &["Cy", "Dana", "Eve"],
            ),
            ("EXISTS { MATCH (person)-[:WORKS_AT]->() }", &["Cy"], &[]),
            (
                "NOT (person)-[:WORKS_AT]->()",
                &["Ann", "Bob", "Dana", "Eve"],
                &["Ann", "Bob", "Cy", "Dana", "Eve"],
            ),
        ];
        for &(predicate, full_names, masked_names) in cases {
            let statement = format!(
                "MATCH (n:Person) WITH collect(n) AS people UNWIND people AS person \
                 WHERE {predicate} RETURN person.name AS name ORDER BY name"
            );
            let full = owner
                .execute(cx, ExecuteMode::Read, &statement, vec![])
                .await
                .unwrap();
            assert_eq!(full.rows, names(full_names), "{predicate}");
            let direct = masked
                .execute(cx, ExecuteMode::Read, &statement, vec![])
                .await
                .unwrap();
            assert_eq!(direct.columns, ["name"]);
            assert_eq!(direct.rows, names(masked_names), "{predicate}");
            let prepared = masked.prepare_read(cx, &statement, vec![]).await.unwrap();
            assert_eq!(
                masked.execute_prepared(cx, prepared, vec![]).await.unwrap(),
                direct,
                "{predicate}",
            );
        }

        masked.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}
