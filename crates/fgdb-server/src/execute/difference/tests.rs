use super::*;
use crate::recovery::tests::{fail_after_marker, served, settled};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::{GqlBudgetDimension, GqlBudgetExceeded, GraphSymbolKind};
use fgdb_protocol::body::{Prepare, PrepareMode};
use fgdb_types::{CanonicalScalar, VId};
use fgdb_warden::{Grant, QueryLimits, Scope};
use std::sync::Arc;

fn accepted<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

fn grant(max_rows: u64) -> Grant {
    let mut grant = Grant::read_only(
        TRUNK,
        u64::MAX / 2,
        QueryLimits {
            max_nodes: 100_000,
            max_work: 100_000_000,
            max_rows,
        },
    );
    grant.labels = Scope::only([LabelId(1)]);
    grant.properties = Scope::only([PropertyKeyId(1)]);
    grant
}

#[test]
fn buffered_difference_preserves_masking_multiplicity_and_exact_aggregate_domains() {
    let ((), report) = run_async_under_lab(0xd1ff_9001, |root| async move {
        let (mut server, _, _) = served(&root, "buffered-difference-values").await;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .symbols
            .bind(GraphSymbolKind::Property, "p", 1)
            .unwrap();
        let db = &server.databases["test"];
        let token = db.authority.issue_at(&grant(100), unix_millis()).unwrap();
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let mut initial = fgdb::WriteBatch::new(RelationId(1));
        for (id, value) in [(1, 3), (2, 3), (3, 7), (99, 999)] {
            initial.create_vertex(
                VId(id),
                vec![LabelId(if id == 99 { 99 } else { 1 })],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
            );
        }
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&commit, initial)
            .await
            .unwrap();
        let mut update = fgdb::WriteBatch::new(RelationId(1));
        update.delete_vertex(VId(1));
        update.delete_vertex(VId(2));
        update.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
        update.set_vertex_property(VId(99), PropertyKeyId(1), Some(CanonicalScalar::Int(888)));
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&commit, update)
            .await
            .unwrap();

        let rows = "MATCH (n) RETURN n.p AS p";
        let result = accepted(read_difference(&root, db, &token, rows, &[], 1, 2).await);
        assert_eq!(result.columns, ["p"]);
        assert_eq!((result.before, result.after), (1, 2));
        assert_eq!(
            result.entries,
            [
                (-2, vec![WireValue::Int(3)]),
                (-1, vec![WireValue::Int(7)]),
                (1, vec![WireValue::Int(9)]),
            ]
        );
        let reverse = accepted(read_difference(&root, db, &token, rows, &[], 2, 1).await);
        assert_eq!(
            reverse.entries,
            result
                .entries
                .iter()
                .map(|(weight, row)| (-weight, row.clone()))
                .collect::<Vec<_>>()
        );
        assert!(
            accepted(read_difference(&root, db, &token, rows, &[], 2, 2).await)
                .entries
                .is_empty()
        );

        let aggregate = accepted(read_difference(
            &root,
            db,
            &token,
            "MATCH (n) RETURN COUNT(*) AS n, SUM(n.p) AS total, AVG(n.p) AS mean, COLLECT(n.p) AS items",
            &[],
            1,
            2,
        ).await);
        assert_eq!(aggregate.columns, ["n", "total", "mean", "items"]);
        assert_eq!(aggregate.entries.len(), 2);
        assert!(aggregate.entries.contains(&(
            -1,
            vec![
                WireValue::Count(3),
                WireValue::WideInt(13),
                WireValue::Average {
                    numerator: 13,
                    denominator: 3
                },
                WireValue::List(vec![
                    WireValue::Int(3),
                    WireValue::Int(3),
                    WireValue::Int(7)
                ]),
            ]
        )));
        assert!(aggregate.entries.contains(&(
            1,
            vec![
                WireValue::Count(1),
                WireValue::WideInt(9),
                WireValue::Average {
                    numerator: 9,
                    denominator: 1
                },
                WireValue::List(vec![WireValue::Int(9)]),
            ]
        )));

        let limited = db.authority.issue_at(&grant(1), unix_millis()).unwrap();
        assert_eq!(
            read_difference(&root, db, &limited, rows, &[], 1, 2)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Budget
        );
        assert_eq!(
            read_difference(&root, db, &token, rows, &[], 1, 3)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Execution
        );
        assert_eq!(
            read_difference(
                &root,
                db,
                &token,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p",
                &[],
                1,
                2,
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::Statement
        );

        let text = "MATCH (n) WHERE n.p >= $minimum RETURN n.p AS p";
        let prepared = accepted(
            prepare_read(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::Read,
                    statement: text.into(),
                    parameters: vec![("minimum".into(), WireValue::Int(0))],
                },
            )
            .await,
        );
        let parameters = [("minimum".into(), WireValue::Int(7))];
        let prepared_result = accepted(
            read_prepared_difference(&root, db, &token, &prepared, &parameters, 1, 2).await,
        );
        assert_eq!(
            prepared_result.entries,
            [(-1, vec![WireValue::Int(7)]), (1, vec![WireValue::Int(9)])]
        );
        let text_result =
            accepted(read_difference(&root, db, &token, text, &parameters, 1, 2).await);
        assert_eq!(prepared_result.entries, text_result.entries);

        let mut expired_grant = grant(100);
        expired_grant.expires_at_ms = 1;
        let expired = db.authority.issue_at(&expired_grant, 0).unwrap();
        let invalid = [("minimum".into(), WireValue::Vertex(1))];
        assert_eq!(
            read_difference(&root, db, &expired, "invalid syntax", &invalid, 9, 0)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            read_prepared_difference(&root, db, &expired, &prepared, &invalid, 9, 0)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::PermissionDenied
        );
        fail_after_marker(&root, db, 100).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { .. }
        ));
        assert!(result.generation.check().is_err());
        assert_eq!(
            read_prepared_difference(&root, db, &token, &prepared, &parameters, 1, 2)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::DatabaseRecovering
        );
        // Authentication continues to win over a stale template and malformed
        // arguments after recovery has installed a different source owner.
        assert_eq!(
            read_prepared_difference(&root, db, &expired, &prepared, &invalid, 9, 0)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::PermissionDenied
        );
        let fresh = accepted(read_difference(&root, db, &token, rows, &[], 1, 2).await);
        assert_eq!(fresh.entries, result.entries);
        assert!(fresh.generation.check().is_ok());
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_weights_never_narrow_and_nested_refusals_keep_their_error_class() {
    for value in [i128::MIN, -1, 0, 1, i128::MAX] {
        assert_eq!(accepted(wire_weight(&ZWeight::from_i128(value))), value);
    }
    let promoted = ZWeight::from_i128(i128::MAX)
        .checked_add(&ZWeight::ONE, LimbLimit::new(2))
        .unwrap();
    assert_eq!(
        wire_weight(&promoted).err().unwrap().code,
        ErrorCode::Execution
    );
    for error in [
        GqlQueryError::Interrupted(QueryError::Authorization(fgdb_warden::Error::Expired)),
        GqlQueryError::Source(GraphDiffError::Definition(QueryError::Authorization(
            fgdb_warden::Error::WrongAuthority,
        ))),
        GqlQueryError::Source(GraphDiffError::Endpoint {
            endpoint: fgdb_gql::result_diff::DiffEndpoint::After,
            source: QueryError::Authorization(fgdb_warden::Error::ScopeDenied),
        }),
    ] {
        assert_eq!(difference_refusal(error).code, ErrorCode::PermissionDenied);
    }
    assert_eq!(
        difference_refusal(GqlQueryError::Rows(GqlBudgetExceeded {
            dimension: GqlBudgetDimension::ResultRows,
            limit: 1,
            observed: 2,
        }))
        .code,
        ErrorCode::Budget
    );
    assert_eq!(
        difference_refusal(GqlQueryError::Source(GraphDiffError::TemporalSelector)).code,
        ErrorCode::Statement
    );
}
