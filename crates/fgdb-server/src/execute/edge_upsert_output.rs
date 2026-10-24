//! Prepare shared wire cells before any authorized write-RETURN commits.
//! The protocol's existing codec owns body admission, not a second value codec.

use super::{Refusal, convert, write_refusal};
use fgdb::WriteTxnError;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_protocol::body::{
    Body, BodyError, ErrorCode, MAX_COLUMNS, MAX_NAME_BYTES, ResultChunk, WireValue,
};

mod admission;

fn body_refusal(error: BodyError) -> Refusal {
    let code = match error {
        BodyError::TooLarge | BodyError::TooDeep => ErrorCode::Budget,
        _ => ErrorCode::Execution,
    };
    // Structural diagnostic only: never include a field value or column text.
    Refusal::new(code, error.to_string())
}

/// The native caller retains its rollback workspace and live permit. Only
/// completed wire cells escape this helper; a later row failure exposes none.
/// Admit every borrowed value node and payload before converting the row.
/// Copy work and the codec's encoded-byte work share the live signed permit.
/// The codec may retain a temporary row-sized encoding; this is not a new
/// byte-memory bound. Negotiated frame sizes, flow credit, delivery/revocation
/// and I/O failures remain transport responsibilities after native completion.
pub(super) fn collect(
    columns: &[String],
    input: &[GraphValueRow],
    work: &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
) -> Result<Vec<Vec<WireValue>>, Refusal> {
    let mut charge = |units| work(units).map_err(|error| write_refusal(&error));
    charge(1)?;
    if columns.len() > MAX_COLUMNS {
        return Err(body_refusal(BodyError::TooLarge));
    }
    for column in columns {
        if column.len() > MAX_NAME_BYTES {
            return Err(body_refusal(BodyError::TooLarge));
        }
        let units = u64::try_from(column.len().div_ceil(64))
            .map_err(|_| body_refusal(BodyError::TooLarge))?;
        charge(units.saturating_add(1))?;
    }
    // Even an empty result carries its schema. Use the same structural codec
    // as actual delivery so invalid names cannot fail only after a write lands.
    ResultChunk {
        columns: Some(columns.to_vec()),
        rows: Vec::new(),
    }
    .encode()
    .map_err(body_refusal)?;
    let mut rows = Vec::new();
    for row in input {
        charge(1)?;
        if row.values().len() != columns.len() {
            return Err(Refusal::new(
                ErrorCode::Execution,
                "native RETURN row width mismatch",
            ));
        }
        // The old per-cell charge could admit an arbitrarily large nested
        // value for one unit, then bill its payload only AFTER copying it.
        // Validate and reserve the entire borrowed row before any wire copy.
        admission::row(row.values(), &mut charge)?;
        let cells: Vec<_> = row.values().iter().map(convert::graph).collect();
        let bytes = ResultChunk::row_len(&cells).map_err(body_refusal)?;
        let units =
            u64::try_from(bytes.div_ceil(64)).map_err(|_| body_refusal(BodyError::TooLarge))?;
        charge(units)?;
        rows.push(cells);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::{Answer, read, write};
    use crate::recovery::tests::served;
    use crate::{Served, TRUNK, unix_millis};
    use asupersync::{Cx, lab::run_async_under_lab};
    use fgdb::WriteBatch;
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
    use fgdb_gql::GraphSymbolKind;
    use fgdb_protocol::body::{Execute, ExecuteMode, MAX_NAME_BYTES, Outcome};
    use fgdb_types::{CanonicalScalar, CommitSeq, EId, PurposeContexts, VId};
    use fgdb_warden::{CapabilityToken, Grant, QueryLimits, Rights, Scope};
    use std::sync::Arc;

    const R: RelationId = RelationId(1);
    const S: RelationId = RelationId(2);
    const L: LabelId = LabelId(1);
    const P: PropertyKeyId = PropertyKeyId(1);
    const SECRET: PropertyKeyId = PropertyKeyId(2);
    const FIRST: EId = EId((1_u128 << 96) + 1);
    const SECOND: EId = EId((1_u128 << 112) + 2);
    const PREFIX: &str = "MATCH (a:Visible)-[s:S]->(b:Visible) MERGE (a)-[r:R]-(b)";
    const ACTIONS: &str = "ON CREATE SET r.p=7 ON MATCH SET r.p=r.p+1";

    fn accepted(result: Result<Answer, Refusal>) -> Answer {
        result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
    }

    async fn fixture(cx: &Cx, name: &str, existing: bool) -> crate::Server {
        let (mut server, _, _) = served(cx, name).await;
        let db = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        for (kind, name, id) in [
            (GraphSymbolKind::Label, "Visible", 1),
            (GraphSymbolKind::Relation, "R", 1),
            (GraphSymbolKind::Relation, "S", 2),
            (GraphSymbolKind::Property, "p", 1),
            (GraphSymbolKind::Property, "secret", 2),
        ] {
            db.symbols.bind(kind, name, id).unwrap();
        }
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let mut guard = db.db.write(cx).await.unwrap();
        let mut seed = WriteBatch::new(S);
        seed.create_vertex(VId(1), vec![L], vec![]);
        seed.create_vertex(VId(2), vec![L], vec![]);
        seed.create_vertex(VId(3), vec![LabelId(2)], vec![]);
        for id in [1, 2] {
            seed.add_edge(EId(id), VId(1), VId(2), vec![]);
        }
        seed.add_edge(EId(3), VId(1), VId(3), vec![]);
        guard.write(&contexts.commit(), seed).await.unwrap();
        if existing {
            let mut seed = WriteBatch::new(R);
            for (edge, src, dst, p) in [(FIRST, 1, 2, 10), (SECOND, 2, 1, 20)] {
                seed.add_edge(
                    edge,
                    VId(src),
                    VId(dst),
                    vec![
                        (P, CanonicalScalar::Int(p)),
                        (SECRET, CanonicalScalar::ucs_basic_text("private").unwrap()),
                    ],
                );
            }
            guard.write(&contexts.commit(), seed).await.unwrap();
        }
        drop(guard);
        server
    }

    fn token(db: &Served, rows: u64) -> CapabilityToken {
        db.authority
            .issue_at(
                &Grant {
                    branch: TRUNK.into(),
                    labels: Scope::only([L]),
                    relations: Scope::only([R, S]),
                    properties: Scope::only([P]),
                    rights: Rights::ReadWrite,
                    limits: QueryLimits {
                        max_nodes: 1_000_000,
                        max_work: 100_000_000,
                        max_rows: rows,
                    },
                    expires_at_ms: u64::MAX / 2,
                },
                unix_millis(),
            )
            .unwrap()
    }

    fn request(tail: &str) -> Execute {
        Execute {
            mode: ExecuteMode::Write,
            statement: format!("{PREFIX} {ACTIONS} {tail}"),
            parameters: Vec::new(),
        }
    }

    async fn state(cx: &Cx, db: &Served) -> (CommitSeq, Vec<fgdb::EdgeRecord>) {
        let guard = db.db.read(cx).await.unwrap();
        (guard.frontier().unwrap(), guard.edges().unwrap())
    }

    #[test]
    fn shared_server_returns_every_parallel_occurrence_then_one_aggregate() {
        let ((), report) = run_async_under_lab(0xe8c0_0101, |root| async move {
            let server = fixture(&root, "edge-upsert-output-bag", true).await;
            let db = &server.databases["test"];
            let before = state(&root, db).await.0;
            let answer = accepted(
                write(
                    &root,
                    db,
                    &token(db, 4),
                    &request("RETURN r AS edge,r.p AS p,r.secret AS hidden ORDER BY edge"),
                )
                .await,
            );
            assert_eq!(answer.columns, vec!["edge", "p", "hidden"]);
            assert_eq!(
                answer.rows,
                vec![
                    vec![
                        WireValue::Edge(FIRST.0),
                        WireValue::Int(12),
                        WireValue::Null
                    ],
                    vec![
                        WireValue::Edge(FIRST.0),
                        WireValue::Int(12),
                        WireValue::Null
                    ],
                    vec![
                        WireValue::Edge(SECOND.0),
                        WireValue::Int(22),
                        WireValue::Null
                    ],
                    vec![
                        WireValue::Edge(SECOND.0),
                        WireValue::Int(22),
                        WireValue::Null
                    ],
                ]
            );
            assert!(
                matches!(answer.outcome, Outcome::WriteCommitted { seq, statements: 1 } if seq == before.0 + 1)
            );
            let encoded = ResultChunk {
                columns: Some(answer.columns),
                rows: answer.rows,
            }
            .encode()
            .unwrap();
            assert_eq!(ResultChunk::decode(&encoded).unwrap().rows.len(), 4);
            let answer = accepted(
                write(
                    &root,
                    db,
                    &token(db, 1),
                    &request("WITH r.p AS p RETURN count(*) AS n,sum(p) AS total"),
                )
                .await,
            );
            assert_eq!(
                answer.rows,
                vec![vec![WireValue::Int(4), WireValue::Int(76)]]
            );
            assert!(
                matches!(answer.outcome, Outcome::WriteCommitted { seq, statements: 1 } if seq == before.0 + 2)
            );
            let guard = db.db.read(&root).await.unwrap();
            for (edge, expected) in [(FIRST, 14), (SECOND, 24)] {
                assert_eq!(
                    guard.edge(edge).unwrap().unwrap().props,
                    vec![
                        (P, CanonicalScalar::Int(expected).into()),
                        (
                            SECRET,
                            CanonicalScalar::ucs_basic_text("private").unwrap().into(),
                        ),
                    ]
                );
            }
            drop(guard);
            server.join_database_workers(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn encoder_boundary_validates_empty_schemas_and_polls_host_work() {
        // This tests the host helper's codec boundary, not a text query: the
        // current native compiler has a stricter column-name limit than FGP.
        let valid = vec!["x".repeat(MAX_NAME_BYTES)];
        let mut charged = 0;
        let rows = collect(&valid, &[], &mut |work| {
            charged += work;
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert!(rows.is_empty());
        assert_eq!(charged, 1 + 1 + (MAX_NAME_BYTES as u64).div_ceil(64));
        let invalid = vec!["x".repeat(MAX_NAME_BYTES + 1)];
        let error = collect(&invalid, &[], &mut |_| Ok(()))
            .expect_err("even a zero-row answer must validate its schema");
        assert_eq!(error.code, ErrorCode::Budget);
        assert!(!error.message.contains(&invalid[0]));
        let mut polls = 0;
        let error = collect(&valid, &[], &mut |_| {
            polls += 1;
            Err(WriteTxnError::AuthorizedMutationRefused)
        })
        .expect_err("host work refusal cannot be ignored");
        assert_eq!(polls, 1);
        assert_eq!(error.code, ErrorCode::PermissionDenied);
    }

    #[test]
    fn shared_server_preserves_late_refusals_signed_rows_and_filtered_effects() {
        let ((), report) = run_async_under_lab(0xe8c0_0103, |root| async move {
            let server = fixture(&root, "edge-output-limits", true).await;
            let db = &server.databases["test"];
            let before = state(&root, db).await;
            for (tail, rows) in [("RETURN r", 3), ("RETURN 1/(r.p-22) AS bad LIMIT 0", 0)] {
                assert!(
                    write(&root, db, &token(db, rows), &request(tail))
                        .await
                        .is_err()
                );
                assert_eq!(state(&root, db).await, before);
            }
            let mut statement = request("RETURN r.p AS p");
            statement.mode = ExecuteMode::Read;
            assert!(read(&root, db, &token(db, 4), &statement).await.is_err());
            assert_eq!(state(&root, db).await, before);
            let answer = accepted(
                write(
                    &root,
                    db,
                    &token(db, 0),
                    &request("WITH r.p AS p WHERE p < 0 RETURN p"),
                )
                .await,
            );
            assert_eq!(answer.columns, vec!["p"]);
            assert!(answer.rows.is_empty());
            assert!(
                matches!(answer.outcome, Outcome::WriteCommitted { seq, statements: 1 } if seq == (before.0).0 + 1)
            );
            let guard = db.db.read(&root).await.unwrap();
            for (edge, expected) in [(FIRST, 12), (SECOND, 22)] {
                assert!(
                    guard
                        .edge(edge)
                        .unwrap()
                        .unwrap()
                        .props
                        .contains(&(P, CanonicalScalar::Int(expected).into()))
                );
            }
            drop(guard);
            server.join_database_workers(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
