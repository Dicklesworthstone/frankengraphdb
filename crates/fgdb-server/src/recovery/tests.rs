//! Real Chronicle/UnixVfs recovery under the deterministic host scheduler.
//! A surviving unsynced marker models surviving bytes, not a power-loss VFS.

use super::*;
use crate::{DatabaseConfig, MaintenanceError, Served, Server, ServerLimits};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{CrashPoint, DatabaseKeys, DerivedPublicationStage, WriteBatch, WriteError};
use fgdb_delta_types::RelationId;
use fgdb_protocol::body::{Execute, ExecuteMode};
use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, VId};
use fgdb_warden::{CapabilityToken, Grant, QueryLimits, Rights, Scope};
use std::future::Future;
use std::path::PathBuf;

fn catalog_accept<T>(result: Result<T, crate::execute::Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

fn catalog_request(mode: ExecuteMode, statement: &str) -> Execute {
    Execute {
        mode,
        statement: statement.to_owned(),
        parameters: vec![],
    }
}

#[derive(Default)]
pub(crate) struct WakeCounter(pub(crate) AtomicUsize);
impl std::task::Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// The real adapters write into this bounded-poll byte sink. Pending writes
/// do not wake themselves, so recovery must supply the wake under test.
pub(crate) struct WireIo {
    state: Arc<WireState>,
    writes: std::collections::VecDeque<usize>,
    flush_pending: bool,
}

pub(crate) struct WireState {
    bytes: std::sync::Mutex<Vec<u8>>,
    pub(crate) writes: AtomicUsize,
    pub(crate) flushes: AtomicUsize,
}

impl WireState {
    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }
}

impl WireIo {
    pub(crate) fn new(
        writes: impl IntoIterator<Item = usize>,
        flush_pending: bool,
    ) -> (Self, Arc<WireState>) {
        let state = Arc::new(WireState {
            bytes: std::sync::Mutex::new(Vec::new()),
            writes: AtomicUsize::new(0),
            flushes: AtomicUsize::new(0),
        });
        (
            Self {
                state: Arc::clone(&state),
                writes: writes.into_iter().collect(),
                flush_pending,
            },
            state,
        )
    }
}

impl asupersync::io::AsyncRead for WireIo {
    fn poll_read(
        self: core::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut asupersync::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
}

impl asupersync::io::AsyncWrite for WireIo {
    fn poll_write(
        mut self: core::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.state.writes.fetch_add(1, Ordering::Relaxed);
        let count = self
            .writes
            .pop_front()
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        if count == 0 {
            return Poll::Pending;
        }
        self.state
            .bytes
            .lock()
            .unwrap()
            .extend_from_slice(&bytes[..count]);
        Poll::Ready(Ok(count))
    }
    fn poll_flush(
        mut self: core::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.state.flushes.fetch_add(1, Ordering::Relaxed);
        if self.flush_pending {
            self.flush_pending = false;
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
    fn poll_shutdown(
        self: core::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

pub(crate) async fn served(cx: &Cx, name: &str) -> (Server, CapabilityToken, PathBuf) {
    served_with_config(cx, name, |_| {}).await
}

async fn served_catalog(
    cx: &Cx,
    name: &str,
    configure: impl FnOnce(&mut DatabaseConfig),
) -> (Server, CapabilityToken, PathBuf) {
    use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_types::{CanonicalScalar, EId};

    let (initial, token, path) = served(cx, name).await;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    {
        let mut writer = initial.databases["test"].db.write(cx).await.unwrap();
        let mut catalog = SymbolCatalog::default();
        catalog
            .bind_all([
                ("Person", CatalogSymbol::Label(LabelId(1))),
                ("Secret", CatalogSymbol::Label(LabelId(2))),
                ("KNOWS", CatalogSymbol::Relation(RelationId(3))),
                ("SECRET_REL", CatalogSymbol::Relation(RelationId(4))),
                ("score", CatalogSymbol::Property(PropertyKeyId(1))),
                ("secret", CatalogSymbol::Property(PropertyKeyId(2))),
            ])
            .unwrap();
        assert_eq!(
            writer
                .register_symbols(&contexts.commit(), &catalog)
                .await
                .unwrap(),
            CommitSeq(1)
        );
        let mut graph = WriteBatch::new(RelationId(3));
        graph.create_vertex(
            VId(1),
            vec![LabelId(1), LabelId(2)],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(42)),
                (PropertyKeyId(2), CanonicalScalar::Int(999)),
            ],
        );
        graph.create_vertex(
            VId(2),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
        );
        graph.add_edge(EId(1), VId(1), VId(2), vec![]);
        assert_eq!(
            writer.write(&contexts.commit(), graph).await.unwrap(),
            CommitSeq(2)
        );
    }
    initial.finish_shutdown(cx).await;
    drop(initial);
    let mut server = Server::new(cx, ServerLimits::default()).unwrap();
    let keys = DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    );
    let mut config = DatabaseConfig::new("test", keys, AuthKey::from_seed(791));
    configure(&mut config);
    server.open_database(cx, &path, config).await.unwrap();
    (server, token, path)
}

async fn served_with_config(
    cx: &Cx,
    name: &str,
    configure: impl FnOnce(&mut DatabaseConfig),
) -> (Server, CapabilityToken, PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "fgdb-server-recovery-{}-{name}",
        std::process::id()
    ));
    let keys = DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    );
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    drop(
        Database::create(&contexts.commit(), &path, keys.clone())
            .await
            .unwrap(),
    );
    let mut server = Server::new(cx, ServerLimits::default()).unwrap();
    let mut config = DatabaseConfig::new("test", keys, AuthKey::from_seed(791));
    configure(&mut config);
    server.open_database(cx, &path, config).await.unwrap();
    let db = &server.databases["test"];
    let token = db
        .authority
        .issue_at(
            &Grant {
                branch: crate::TRUNK.into(),
                labels: Scope::All,
                relations: Scope::All,
                properties: Scope::All,
                rights: Rights::ReadWrite,
                limits: QueryLimits {
                    max_nodes: 1_000_000,
                    max_work: 100_000_000,
                    max_rows: 1_000_000,
                },
                expires_at_ms: u64::MAX / 2,
            },
            crate::unix_millis(),
        )
        .unwrap();
    (server, token, path)
}

pub(crate) fn vertex(id: u128) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(VId(id), vec![], vec![]);
    batch
}

pub(crate) async fn fail_after_marker(cx: &Cx, db: &Served, id: u128) -> WriteError {
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let mut guard = db.db.write(cx).await.unwrap();
    let prepared = guard.prepare_atomic_writes(vec![vertex(id)]).unwrap();
    let error = guard
        .commit_prepared_with_crash(
            &contexts.commit(),
            prepared,
            Some(CrashPoint::AfterMarkerBeforeD2),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, WriteError::CommitOutcomeUnknown { .. }));
    assert_eq!(
        crate::execute::write_refusal(&error).code,
        ErrorCode::OutcomeUnknown
    );
    drop(guard);
    error
}

pub(crate) async fn settled(db: &Served) -> DatabaseStatus {
    let mut watcher = db.commits.watcher();
    poll_fn(|task| {
        loop {
            let state = db.db.status();
            if !matches!(state, DatabaseStatus::Recovering { .. }) {
                return Poll::Ready(state);
            }
            if !watcher.poll_changed(task) {
                return Poll::Pending;
            }
        }
    })
    .await
}

#[test]
fn durable_catalog_opens_named_reads_preparation_and_writes_without_configuration() {
    use fgdb_protocol::body::{Outcome, Prepare, PrepareMode, WireValue};

    let ((), report) = run_async_under_lab(0x79a0_ca01, |root| async move {
        let (server, token, _) = served_catalog(&root, "catalog-startup", |_| {}).await;
        let db = &server.databases["test"];
        let request = catalog_request(
            ExecuteMode::Read,
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.score AS source, type(r) AS relation, b.score AS target",
        );
        let answer = catalog_accept(crate::execute::read(&root, db, &token, &request).await);
        assert_eq!(answer.outcome, Outcome::Rows { seq: 2 });
        assert_eq!(
            answer.rows,
            [vec![
                WireValue::Int(42),
                WireValue::Text("KNOWS".into()),
                WireValue::Int(7)
            ]]
        );
        let prepared = catalog_accept(
            crate::execute::prepare_read(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::Read,
                    statement: "MATCH (n:Person) RETURN n.score AS score ORDER BY score".into(),
                    parameters: vec![],
                },
            )
            .await,
        );
        assert_eq!(
            catalog_accept(crate::execute::read_prepared(&root, db, &token, &prepared, &[]).await)
                .rows,
            [vec![WireValue::Int(7)], vec![WireValue::Int(42)]]
        );
        let written = catalog_accept(
            crate::execute::write(
                &root,
                db,
                &token,
                &catalog_request(
                    ExecuteMode::Write,
                    "CREATE (n:Person {score:99}) RETURN n.score AS score",
                ),
            )
            .await,
        );
        assert_eq!(written.rows, [vec![WireValue::Int(99)]]);
        assert_eq!(written.created.vertices, 1);
        assert_eq!(
            catalog_accept(crate::execute::read_prepared(&root, db, &token, &prepared, &[]).await)
                .rows,
            [
                vec![WireValue::Int(7)],
                vec![WireValue::Int(42)],
                vec![WireValue::Int(99)]
            ]
        );
        server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn durable_catalog_startup_conflicts_do_not_change_the_database_or_hold_its_writer() {
    use fgdb_gql::GraphSymbolKind;

    let ((), report) = run_async_under_lab(0x79a0_ca02, |root| async move {
        let (server, _, path) = served_catalog(&root, "catalog-startup-conflicts", |_| {}).await;
        server.finish_shutdown(&root).await;
        drop(server);
        let before = std::fs::read(path.join("manifest.root")).unwrap();
        for (kind, name, id) in [
            (GraphSymbolKind::Label, "Person", 99),
            (GraphSymbolKind::Label, "Impostor", 1),
            (GraphSymbolKind::Relation, "KNOWS", 99),
            (GraphSymbolKind::Property, "score", 99),
        ] {
            let mut attempt = Server::new(&root, ServerLimits::default()).unwrap();
            let keys = DatabaseKeys::new(
                [0x91; 32],
                DatabaseSecurityNamespaceId([0x92; 32]),
                [0x93; 32],
            );
            let mut config = DatabaseConfig::new("test", keys.clone(), AuthKey::from_seed(791));
            config.symbols.bind(kind, name, id).unwrap();
            assert!(matches!(
                attempt.open_database(&root, &path, config).await,
                Err(crate::ServerError::Symbols(_))
            ));
            assert!(!attempt.databases.contains_key("test"));
            assert_eq!(std::fs::read(path.join("manifest.root")).unwrap(), before);
            attempt
                .open_database(
                    &root,
                    &path,
                    DatabaseConfig::new("test", keys, AuthKey::from_seed(791)),
                )
                .await
                .unwrap();
            assert_eq!(
                attempt.databases["test"]
                    .db
                    .read(&root)
                    .await
                    .unwrap()
                    .frontier()
                    .unwrap(),
                CommitSeq(2)
            );
            attempt.finish_shutdown(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn durable_catalog_http_schema_masks_names_and_configured_extras_stay_ephemeral() {
    use asupersync::http::h1::types::{Method, RequestBuilder};
    use fgdb_delta_types::catalog::CatalogSymbolKind;
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_gql::GraphSymbolKind;
    use fgdb_warden::Restriction;

    let ((), report) = run_async_under_lab(0x79a0_ca03, |root| async move {
        let (server, token, path) = served_catalog(&root, "catalog-masking", |config| {
            config
                .symbols
                .bind(GraphSymbolKind::Label, "Person", 1)
                .unwrap();
            config
                .symbols
                .bind(GraphSymbolKind::Property, "local_only", 50)
                .unwrap();
        })
        .await;
        let limited = token
            .attenuate(Restriction::Labels(Scope::only([LabelId(1)])))
            .unwrap()
            .attenuate(Restriction::Relations(Scope::only([RelationId(3)])))
            .unwrap()
            .attenuate(Restriction::Properties(Scope::only([PropertyKeyId(1)])))
            .unwrap();
        for (capability, expected) in [
            (
                &token,
                r#"{"v":1,"labels":["Person","Secret"],"relations":["KNOWS","SECRET_REL"],"properties":["local_only","score","secret"]}"#,
            ),
            (
                &limited,
                r#"{"v":1,"labels":["Person"],"relations":["KNOWS"],"properties":["score"]}"#,
            ),
        ] {
            let request = RequestBuilder::new(Method::Get, "/v1/databases/test/schema")
                .header(
                    "Authorization",
                    format!("Bearer {}", fgdb_protocol::json::hex(&capability.encode())),
                )
                .build();
            let response = crate::http::respond(
                &root,
                &server,
                request,
                &crate::http::OutputAuthority::new(),
            )
            .await;
            assert_eq!(response.status, 200);
            assert_eq!(String::from_utf8(response.body).unwrap(), expected);
        }
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = &server.databases["test"];
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(2)
        );
        server.finish_shutdown(&root).await;
        drop(server);
        let keys = DatabaseKeys::new(
            [0x91; 32],
            DatabaseSecurityNamespaceId([0x92; 32]),
            [0x93; 32],
        );
        let reopened = Database::open(&contexts.commit(), &path, keys)
            .await
            .unwrap();
        assert_eq!(
            reopened
                .symbol_catalog()
                .unwrap()
                .resolve(CatalogSymbolKind::Property, "local_only"),
            None
        );
        assert_eq!(reopened.frontier().unwrap(), CommitSeq(2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn durable_catalog_refresh_preserves_pinned_and_historical_name_visibility() {
    use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_protocol::body::{Outcome, WireValue};
    use fgdb_types::CanonicalScalar;

    let ((), report) = run_async_under_lab(0x79a0_ca04, |root| async move {
        let (server, token, _) = served_catalog(&root, "catalog-refresh-history", |_| {}).await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (mut pinned, pinned_at) =
            catalog_accept(crate::execute::read_session(&root, db, &token).await);
        assert_eq!(pinned_at, CommitSeq(2));
        // The server opened after the first schema commit. Its startup names
        // must still be absent when preparing against the origin snapshot.
        let missing = crate::execute::read(
            &root,
            db,
            &token,
            &catalog_request(
                ExecuteMode::Read,
                "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ 0 RETURN n.score",
            ),
        )
        .await
        .err()
        .expect("names committed at sequence one are absent at origin");
        assert_eq!(missing.code, ErrorCode::Statement);
        {
            let mut later = SymbolCatalog::default();
            later
                .bind_all([
                    ("Later", CatalogSymbol::Label(LabelId(8))),
                    ("later", CatalogSymbol::Property(PropertyKeyId(9))),
                ])
                .unwrap();
            let mut writer = db.db.write(&root).await.unwrap();
            assert_eq!(
                writer
                    .register_symbols(&contexts.commit(), &later)
                    .await
                    .unwrap(),
                CommitSeq(3)
            );
            let mut graph = WriteBatch::new(RelationId(3));
            graph.create_vertex(
                VId(3),
                vec![LabelId(8)],
                vec![(PropertyKeyId(9), CanonicalScalar::Int(31))],
            );
            assert_eq!(
                writer.write(&contexts.commit(), graph).await.unwrap(),
                CommitSeq(4)
            );
        }
        let current = catalog_accept(
            crate::execute::read(
                &root,
                db,
                &token,
                &catalog_request(ExecuteMode::Read, "MATCH (n:Later) RETURN n.later AS value"),
            )
            .await,
        );
        assert_eq!(current.outcome, Outcome::Rows { seq: 4 });
        assert_eq!(current.rows, [vec![WireValue::Int(31)]]);
        let schema = catalog_accept(crate::execute::schema(&root, db, &token).await);
        assert_eq!(schema.labels, ["Later", "Person", "Secret"]);
        assert_eq!(schema.properties, ["later", "score", "secret"]);
        let previous = catalog_accept(
            crate::execute::read(
                &root,
                db,
                &token,
                &catalog_request(
                    ExecuteMode::Read,
                    "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.score AS score ORDER BY score",
                ),
            )
            .await,
        );
        assert_eq!(previous.outcome, Outcome::Rows { seq: 2 });
        assert_eq!(
            previous.rows,
            [vec![WireValue::Int(7)], vec![WireValue::Int(42)]]
        );
        let missing = crate::execute::read(
            &root,
            db,
            &token,
            &catalog_request(
                ExecuteMode::Read,
                "MATCH (n:Later) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.later",
            ),
        )
        .await
        .err()
        .expect("a later durable name cannot bind against an earlier catalog");
        assert_eq!(missing.code, ErrorCode::Statement);
        let parameters = fgdb_gql::GqlParameters::new();
        let (_, selected) = pinned
            .query_with_snapshot(
                &contexts.query(),
                "MATCH (n:Person) RETURN n.score AS score ORDER BY score",
                &parameters,
            )
            .unwrap_or_else(|_| panic!("the pinned catalog keeps its original names"));
        assert_eq!(selected, CommitSeq(2));
        assert!(
            pinned
                .query(
                    &contexts.query(),
                    "MATCH (n:Later) RETURN n.later",
                    &parameters
                )
                .is_err(),
            "an already-open session must not acquire a later catalog"
        );
        drop(pinned);
        server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn durable_catalog_recovery_refreshes_names_and_fences_prepared_generations() {
    use fgdb_delta_types::LabelId;
    use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
    use fgdb_protocol::body::{Prepare, PrepareMode, WireValue};

    let ((), report) = run_async_under_lab(0x79a0_ca05, |root| async move {
        let (server, token, _) = served_catalog(&root, "catalog-recovery", |_| {}).await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let prepared = catalog_accept(
            crate::execute::prepare_read(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::Read,
                    statement: "MATCH (n:Person) RETURN n.score AS score ORDER BY score".into(),
                    parameters: vec![],
                },
            )
            .await,
        );
        {
            let mut later = SymbolCatalog::default();
            later
                .bind("Recovered", CatalogSymbol::Label(LabelId(8)))
                .unwrap();
            let mut writer = db.db.write(&root).await.unwrap();
            assert!(matches!(
                writer
                    .register_symbols_with_faults(
                        &contexts.commit(),
                        &later,
                        None,
                        Some(DerivedPublicationStage::PublishPartitionRoot),
                    )
                    .await,
                Err(fgdb::SymbolCatalogError::Write(error))
                    if matches!(*error, WriteError::CommittedNeedsRecovery { .. })
            ));
        }
        assert_eq!(settled(db).await, DatabaseStatus::Ready { generation: 2 });
        let schema = catalog_accept(crate::execute::schema(&root, db, &token).await);
        assert_eq!(schema.labels, ["Person", "Recovered", "Secret"]);
        let old = crate::execute::read_prepared(&root, db, &token, &prepared, &[])
            .await
            .err()
            .expect("recovery invalidates the former preparation generation");
        assert_eq!(old.code, ErrorCode::DatabaseRecovering);
        let fresh = catalog_accept(
            crate::execute::read(
                &root,
                db,
                &token,
                &catalog_request(ExecuteMode::Read, "MATCH (n:Recovered) RETURN n"),
            )
            .await,
        );
        assert!(
            fresh.rows.is_empty(),
            "the recovered schema committed, no graph effect did"
        );
        let original = catalog_accept(
            crate::execute::read(
                &root,
                db,
                &token,
                &catalog_request(
                    ExecuteMode::Read,
                    "MATCH (n:Person) RETURN n.score AS score ORDER BY score",
                ),
            )
            .await,
        );
        assert_eq!(
            original.rows,
            [vec![WireValue::Int(7)], vec![WireValue::Int(42)]]
        );
        server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn durable_catalog_schema_checks_authority_before_and_after_waiting_for_a_writer() {
    use fgdb_warden::Restriction;

    let ((), report) = run_async_under_lab(0x79a0_ca07, |root| async move {
        let (server, token, _) = served_catalog(&root, "catalog-schema-authority", |_| {}).await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut held = db.db.write(&root).await.unwrap();
        held.write(&contexts.commit(), vertex(99)).await.unwrap();
        let denied = token.attenuate(Restriction::Rights(Rights::Write)).unwrap();
        let mut unauthorized = Box::pin(crate::execute::schema(&root, db, &denied));
        match unauthorized
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            Poll::Ready(Err(error)) => assert_eq!(error.code, ErrorCode::PermissionDenied),
            _ => panic!("read rights must be checked before waiting on the writer"),
        }
        drop(unauthorized);
        let mut waiting = Box::pin(crate::execute::schema(&root, db, &token));
        assert!(
            waiting
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending(),
            "no retained view exists until this writer publishes"
        );
        assert!(db.authority.retire());
        drop(held);
        let error = waiting
            .await
            .err()
            .expect("retired authority cannot receive names");
        assert_eq!(
            error.message,
            fgdb_warden::Error::AuthorityRetired.to_string()
        );
        server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn durable_catalog_prepared_reads_bind_the_execution_cut_and_ignore_allocator_floors() {
    use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_protocol::body::{Outcome, Prepare, PrepareMode, WireValue};
    use fgdb_types::CanonicalScalar;

    let ((), report) = run_async_under_lab(0x79a0_ca08, |root| async move {
        let (server, token, _) = served_catalog(&root, "catalog-prepared-cuts", |_| {}).await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let statement = "MATCH (n:Person) RETURN n.score AS score ORDER BY score";
        let current = catalog_accept(
            crate::execute::prepare_read(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::Read,
                    statement: statement.into(),
                    parameters: vec![],
                },
            )
            .await,
        );
        let at = |seq| vec![("at".to_owned(), WireValue::Count(seq))];
        let historical = catalog_accept(
            crate::execute::prepare_read(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::Read,
                    statement: "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.score AS score ORDER BY score".into(),
                    parameters: at(2),
                },
            )
            .await,
        );
        {
            let mut floors = SymbolCatalog::default();
            floors.reserve_through(CatalogSymbol::Property(PropertyKeyId(12345)));
            let mut writer = db.db.write(&root).await.unwrap();
            assert_eq!(
                writer
                    .register_symbols(&contexts.commit(), &floors)
                    .await
                    .unwrap(),
                CommitSeq(3)
            );
        }
        let floor_only =
            catalog_accept(crate::execute::read_prepared(&root, db, &token, &current, &[]).await);
        assert_eq!(floor_only.outcome, Outcome::Rows { seq: 3 });
        assert_eq!(
            floor_only.rows,
            [vec![WireValue::Int(7)], vec![WireValue::Int(42)]]
        );
        let floor_cut = catalog_accept(
            crate::execute::read_prepared(&root, db, &token, &historical, &at(3)).await,
        );
        assert_eq!(floor_cut.outcome, Outcome::Rows { seq: 3 });
        assert_eq!(floor_cut.rows, floor_only.rows);
        {
            let mut graph = WriteBatch::new(RelationId(3));
            graph.create_vertex(
                VId(3),
                vec![LabelId(1)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(99))],
            );
            assert_eq!(
                db.db
                    .write(&root)
                    .await
                    .unwrap()
                    .write(&contexts.commit(), graph)
                    .await
                    .unwrap(),
                CommitSeq(4)
            );
        }
        let data_only =
            catalog_accept(crate::execute::read_prepared(&root, db, &token, &current, &[]).await);
        assert_eq!(data_only.outcome, Outcome::Rows { seq: 4 });
        assert_eq!(
            data_only.rows,
            [
                vec![WireValue::Int(7)],
                vec![WireValue::Int(42)],
                vec![WireValue::Int(99)]
            ]
        );
        {
            let mut later = SymbolCatalog::default();
            later
                .bind("Later", CatalogSymbol::Label(LabelId(8)))
                .unwrap();
            assert_eq!(
                db.db
                    .write(&root)
                    .await
                    .unwrap()
                    .register_symbols(&contexts.commit(), &later)
                    .await
                    .unwrap(),
                CommitSeq(5)
            );
        }
        let changed = crate::execute::read_prepared(&root, db, &token, &current, &[])
            .await
            .err()
            .expect("a current prepared read cannot retain an obsolete name map");
        assert_eq!(changed.code, ErrorCode::Conflict);
        for (seq, expected) in [(2, floor_only.rows), (4, data_only.rows)] {
            let previous = catalog_accept(
                crate::execute::read_prepared(&root, db, &token, &historical, &at(seq)).await,
            );
            assert_eq!(previous.outcome, Outcome::Rows { seq });
            assert_eq!(previous.rows, expected);
        }
        let changed_cut = crate::execute::read_prepared(&root, db, &token, &historical, &at(5))
            .await
            .err()
            .expect("rebinding AS OF must also recheck the selected catalog");
        assert_eq!(changed_cut.code, ErrorCode::Conflict);
        assert_eq!(
            catalog_accept(
                crate::execute::read_prepared(&root, db, &token, &historical, &at(2)).await,
            )
            .outcome,
            Outcome::Rows { seq: 2 },
            "a refused new cut must not mutate the reusable historical template"
        );
        server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn durable_catalog_batches_and_differences_require_compatible_historical_names() {
    use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_protocol::body::{ExecuteBatch, WireValue};
    use fgdb_types::CanonicalScalar;

    let ((), report) = run_async_under_lab(0x79a0_ca09, |root| async move {
        let (server, token, _) = served_catalog(&root, "catalog-mixed-cuts", |_| {}).await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        {
            let mut writer = db.db.write(&root).await.unwrap();
            let mut graph = WriteBatch::new(RelationId(3));
            graph.create_vertex(
                VId(3),
                vec![LabelId(1)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(99))],
            );
            assert_eq!(
                writer.write(&contexts.commit(), graph).await.unwrap(),
                CommitSeq(3)
            );
            let mut later = SymbolCatalog::default();
            later
                .bind("Later", CatalogSymbol::Label(LabelId(8)))
                .unwrap();
            assert_eq!(
                writer
                    .register_symbols(&contexts.commit(), &later)
                    .await
                    .unwrap(),
                CommitSeq(4)
            );
        }
        let batch = |after| {
            ExecuteBatch {
            statement: "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.score AS score ORDER BY score".into(),
            argument_sets: vec![
                vec![("at".into(), WireValue::Count(2))],
                vec![("at".into(), WireValue::Count(after))],
            ],
        }
        };
        let compatible =
            catalog_accept(crate::execute::read_batch(&root, db, &token, &batch(3)).await);
        assert_eq!(compatible.results.len(), 2);
        assert_eq!(compatible.results[0].seq, 2);
        assert_eq!(compatible.results[1].seq, 3);
        assert_eq!(
            compatible.results[0].rows,
            [vec![WireValue::Int(7)], vec![WireValue::Int(42)]]
        );
        assert_eq!(
            compatible.results[1].rows,
            [
                vec![WireValue::Int(7)],
                vec![WireValue::Int(42)],
                vec![WireValue::Int(99)]
            ]
        );
        let statements = |after| {
            vec![
                catalog_request(
                    ExecuteMode::Read,
                    "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.score AS score ORDER BY score",
                ),
                catalog_request(
                    ExecuteMode::Read,
                    &format!(
                        "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ {after} RETURN n.score AS score ORDER BY score"
                    ),
                ),
            ]
        };
        let separate = catalog_accept(
            crate::execute::read_statement_batch(&root, db, &token, &statements(3)).await,
        );
        for (left, right) in compatible.results.iter().zip(&separate.results) {
            assert_eq!(left.seq, right.seq);
            assert_eq!(left.rows, right.rows);
        }
        let statement = "MATCH (n:Person) RETURN n.score AS score";
        let difference = catalog_accept(
            crate::execute::read_difference(&root, db, &token, statement, &[], 2, 3).await,
        );
        assert_eq!(difference.before, 2);
        assert_eq!(difference.after, 3);
        assert_eq!(difference.entries, [(1, vec![WireValue::Int(99)])]);
        let mixed_parameters = crate::execute::read_batch(&root, db, &token, &batch(4))
            .await
            .err()
            .expect("a parameter batch cannot bind one resolver across different catalogs");
        assert_eq!(mixed_parameters.code, ErrorCode::Statement);
        let mixed_statements =
            crate::execute::read_statement_batch(&root, db, &token, &statements(4))
                .await
                .err()
                .expect("a statement batch cannot silently borrow names from another cut");
        assert_eq!(mixed_statements.code, ErrorCode::Statement);
        let mixed_difference =
            crate::execute::read_difference(&root, db, &token, statement, &[], 2, 4)
                .await
                .err()
                .expect("a buffered difference must refuse incompatible endpoint catalogs");
        assert_eq!(mixed_difference.code, ErrorCode::Statement);
        server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn served_cache_limits_and_retained_bytes_follow_authoritative_recovery() {
    let ((), report) = run_async_under_lab(0x79a0_006d, |root| async move {
        let limits = fgdb::ReadCacheLimits {
            projection_bytes: 1 << 20,
            admission_bytes: 1 << 18,
            authorized_projection_bytes: 1 << 19,
            sealed_projection_bytes: 1 << 18,
        };
        let (server, token, _) = served_with_config(&root, "read-cache-owner", |config| {
            config.read_cache_limits = limits;
        })
        .await;
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = &server.databases["test"];
        {
            let mut writer = db.db.write(&root).await.unwrap();
            writer.write(&contexts.commit(), vertex(1)).await.unwrap();
        }
        let (mut old, _) = crate::execute::read_session(&root, db, &token)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        old.query(
            &contexts.query(),
            "MATCH (n) RETURN count(*) AS n",
            &fgdb_gql::GqlParameters::new(),
        )
        .unwrap_or_else(|_| panic!("initial authorized metadata count must succeed"));
        assert_eq!(
            server
                .database_read_cache_status(&root, "test")
                .await
                .unwrap()
                .admissions
                .entries,
            0
        );
        old.query(
            &contexts.query(),
            "MATCH (n) RETURN n",
            &fgdb_gql::GqlParameters::new(),
        )
        .unwrap_or_else(|_| panic!("initial graph scan must populate the admission cache"));
        let before = server
            .database_read_cache_status(&root, "test")
            .await
            .unwrap();
        assert_eq!(before.admissions.entries, 1);
        assert!(before.admissions.retained_bytes > 0);
        assert!(
            server
                .database_read_cache_status(&root, "unknown")
                .await
                .is_none()
        );
        {
            let mut writer = db.db.write(&root).await.unwrap();
            assert!(matches!(
                writer
                    .write_with_publication_failure(
                        &contexts.commit(),
                        vertex(2),
                        DerivedPublicationStage::PublishPartitionRoot,
                    )
                    .await,
                Err(WriteError::CommittedNeedsRecovery { .. })
            ));
        }
        assert_eq!(settled(db).await, DatabaseStatus::Ready { generation: 2 });
        let (mut current, _) = crate::execute::read_session(&root, db, &token)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        current
            .query(
                &contexts.query(),
                "MATCH (n) RETURN count(*) AS n",
                &fgdb_gql::GqlParameters::new(),
            )
            .unwrap_or_else(|_| panic!("recovered authorized metadata count must succeed"));
        current
            .query(
                &contexts.query(),
                "MATCH (n) RETURN n",
                &fgdb_gql::GqlParameters::new(),
            )
            .unwrap_or_else(|_| {
                panic!("recovered graph scan must populate its new snapshot entry")
            });
        let after = server
            .database_read_cache_status(&root, "test")
            .await
            .unwrap();
        assert_eq!(after.projections.max_bytes, limits.projection_bytes);
        assert_eq!(after.admissions.max_bytes, limits.admission_bytes);
        assert_eq!(
            after.authorized_projections.max_bytes,
            limits.authorized_projection_bytes
        );
        assert_eq!(after.admissions.entries, 2);
        drop(old);
        assert_eq!(
            server
                .database_read_cache_status(&root, "test")
                .await
                .unwrap()
                .admissions
                .entries,
            1
        );
        drop(current);
        server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn host_maintenance_preserves_pinned_readers_and_the_committed_frontier() {
    let ((), report) = run_async_under_lab(0x79a0_0010, |root| async move {
        let (server, token, path) = served(&root, "host-maintenance-pins").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(1))
            .await
            .unwrap();
        let (mut pinned, at) = crate::execute::read_session(&root, db, &token)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(at, CommitSeq(1));
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(2))
            .await
            .unwrap();

        let before = std::fs::read(path.join("manifest.root")).unwrap();
        server.compact_database(&root, "test").await.unwrap();
        assert_ne!(std::fs::read(path.join("manifest.root")).unwrap(), before);
        let summary = server.scrub_database(&root, "test").await.unwrap();
        assert_eq!(summary.objects, 2);
        assert_eq!(summary.clean.len() + summary.repaired.len(), 2);
        assert!(summary.metadata_objects >= 2);
        assert!(summary.lost.is_empty());
        assert!(summary.block_lost.is_empty());
        assert!(summary.metadata_lost.is_empty());
        assert_eq!(
            server.database_status(&root, "test"),
            Some(DatabaseStatus::Ready { generation: 1 })
        );
        let (answer, pinned_at) = pinned
            .query_with_snapshot(
                &contexts.query(),
                "MATCH (n) RETURN n",
                &fgdb_gql::GqlParameters::new(),
            )
            .ok()
            .unwrap();
        assert_eq!(pinned_at, CommitSeq(1));
        assert!(matches!(answer, fgdb::QueryResult::Rows { rows, .. } if rows.len() == 1));
        let (fresh, generation) = db.db.read_view(&root).await.unwrap();
        assert_eq!(fresh.frontier(), CommitSeq(2));
        assert!(fresh.vertex(VId(1)).unwrap().is_some());
        assert!(fresh.vertex(VId(2)).unwrap().is_some());
        generation.check().unwrap();
        assert_eq!(
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), vertex(3))
                .await
                .unwrap(),
            CommitSeq(3)
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn host_maintenance_admission_tracks_waiters_cancellation_and_drain() {
    let ((), report) = run_async_under_lab(0x79a0_0011, |root| async move {
        let (server, _, _) = served(&root, "host-maintenance-admission").await;
        assert!(matches!(
            server.compact_database(&root, "unknown").await,
            Err(MaintenanceError::UnknownDatabase(name)) if name == "unknown"
        ));
        assert!(matches!(
            server.scrub_database(&root, "unknown").await,
            Err(MaintenanceError::UnknownDatabase(name)) if name == "unknown"
        ));
        let db = &server.databases["test"];
        let held = db.db.write(&root).await.unwrap();
        let mut maintenance = Box::pin(server.compact_database(&root, "test"));
        assert!(
            maintenance
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        assert_eq!(db.db.shared.gate.state.read().unwrap().active, 1);
        db.db.stop();
        drop(maintenance);
        assert_eq!(db.db.shared.gate.state.read().unwrap().active, 0);
        drop(held);
        server.join_database_workers(&root).await;
        assert!(matches!(
            server.compact_database(&root, "test").await,
            Err(MaintenanceError::DatabaseUnavailable(
                DatabaseStatus::Stopped
            ))
        ));
        assert!(matches!(
            server.scrub_database(&root, "test").await,
            Err(MaintenanceError::DatabaseUnavailable(
                DatabaseStatus::Stopped
            ))
        ));
        let (draining, _, _) = served(&root, "host-maintenance-draining").await;
        draining.shutdown().trigger();
        assert!(matches!(
            draining.compact_database(&root, "test").await,
            Err(MaintenanceError::Draining)
        ));
        assert!(matches!(
            draining.scrub_database(&root, "test").await,
            Err(MaintenanceError::Draining)
        ));
        draining.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn host_scrub_loss_fences_pinned_readers_and_uses_authoritative_recovery() {
    let ((), report) = run_async_under_lab(0x79a0_0012, |root| async move {
        let (server, token, path) = served(&root, "host-scrub-loss").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(1))
            .await
            .unwrap();
        let (mut pinned, _) = crate::execute::read_session(&root, db, &token)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        let admitted = db.db.enter().unwrap();
        // Damage only this test's newly created committed capsule. The scrub
        // must not keep serving the retained graph merely because it is warm.
        let capsules = std::fs::read_dir(path.join("capsules"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(capsules.len(), 1);
        let capsule = &capsules[0];
        let intact = std::fs::read(capsule).unwrap();
        std::fs::write(capsule, b"damaged capsule for host scrub law").unwrap();
        let summary = server.scrub_database(&root, "test").await.unwrap();
        assert_eq!(summary.lost.len(), 1);
        assert_eq!(db.db.status(), DatabaseStatus::Recovering { generation: 2 });
        assert!(matches!(
            pinned.query(
                &contexts.query(),
                "MATCH (n) RETURN n",
                &fgdb_gql::GqlParameters::new(),
            ),
            Err(crate::execute::SessionError::Unavailable(
                Unavailable::Recovering
            ))
        ));
        assert!(matches!(
            server.compact_database(&root, "test").await,
            Err(MaintenanceError::DatabaseUnavailable(
                DatabaseStatus::Recovering { generation: 2 }
            ))
        ));
        drop(admitted);
        let recovered_status = settled(db).await;
        assert!(
            matches!(
                recovered_status,
                DatabaseStatus::Fenced { generation: 2, .. }
            ),
            "lost retained capsule must prevent serving the intact materialized root: {recovered_status:?}"
        );
        std::fs::write(capsule, intact).unwrap();
        assert!(matches!(
            server.scrub_database(&root, "test").await,
            Err(MaintenanceError::DatabaseUnavailable(
                DatabaseStatus::Fenced { generation: 2, .. }
            ))
        ));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn real_marker_boundaries_recover_once_and_preserve_completion_classes() {
    let ((), report) = run_async_under_lab(0x79a0_0001, |root| async move {
        for (name, point, needs_recovery) in [
            ("before-marker", CrashPoint::AfterD1, false),
            ("marker-survived", CrashPoint::AfterMarkerBeforeD2, true),
            (
                "marker-synced",
                CrashPoint::AfterMarkerFileSyncBeforeDirectorySync,
                true,
            ),
        ] {
            let (server, _, _) = served(&root, name).await;
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let old = db.db.generation().unwrap();
            let admitted = db.db.enter().unwrap();
            let mut guard = db.db.write(&root).await.unwrap();
            let prepared = guard.prepare_atomic_writes(vec![vertex(1)]).unwrap();
            let error = guard
                .commit_prepared_with_crash(&contexts.commit(), prepared, Some(point))
                .await
                .unwrap_err();
            drop(guard);
            if needs_recovery {
                assert_eq!(
                    crate::execute::write_refusal(&error).code,
                    ErrorCode::OutcomeUnknown
                );
                assert_eq!(db.db.status(), DatabaseStatus::Recovering { generation: 2 });
                assert!(db.db.shared.gate.state.read().unwrap().published.is_none());
                assert!(matches!(old.enter(), Err(Unavailable::Recovering)));
                assert!(matches!(
                    db.db.read(&root).await,
                    Err(Unavailable::Recovering)
                ));
                assert!(matches!(
                    db.db.read_view(&root).await,
                    Err(Unavailable::Recovering)
                ));
                // The root worker can run, but cannot consume the handle yet.
                asupersync::runtime::yield_now().await;
                assert_eq!(db.db.status(), DatabaseStatus::Recovering { generation: 2 });
            } else {
                assert_ne!(
                    crate::execute::write_refusal(&error).code,
                    ErrorCode::OutcomeUnknown
                );
                assert_eq!(db.db.status(), DatabaseStatus::Ready { generation: 1 });
            }
            drop(admitted);
            let expected_generation = if needs_recovery { 2 } else { 1 };
            assert_eq!(
                settled(db).await,
                DatabaseStatus::Ready {
                    generation: expected_generation
                }
            );
            assert_eq!(old.check().is_err(), needs_recovery);
            let (view, generation) = db.db.read_view(&root).await.unwrap();
            assert_eq!(view.frontier(), CommitSeq(u64::from(needs_recovery)));
            assert_eq!(view.vertex(VId(1)).unwrap().is_some(), needs_recovery);
            generation.check().unwrap();
            drop(view);
            let mut guard = db.db.write(&root).await.unwrap();
            assert_eq!(
                guard.frontier().unwrap(),
                CommitSeq(u64::from(needs_recovery))
            );
            assert_eq!(guard.vertex(VId(1)).unwrap().is_some(), needs_recovery);
            let next = guard.write(&contexts.commit(), vertex(2)).await.unwrap();
            assert_eq!(next, CommitSeq(1 + u64::from(needs_recovery)));
            assert_eq!(
                guard.vertices().unwrap().len(),
                1 + usize::from(needs_recovery)
            );
            drop(guard);
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_d2_recovery_invalidates_pins_and_resets_registration_ownership() {
    let ((), report) = run_async_under_lab(0x79a0_0002, |root| async move {
        let (server, token, _) = served(&root, "post-d2-pins").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (mut pinned, _) = crate::execute::read_session(&root, db, &token)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        let statement = Execute {
            mode: ExecuteMode::Subscribe,
            statement: "SUBSCRIBE TO MATCH (n) RETURN n".into(),
            parameters: vec![],
        };
        let mut old = crate::execute::subscribe(&root, db, &token, &statement)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 1);
        let mut guard = db.db.write(&root).await.unwrap();
        let error = guard
            .write_with_publication_failure(
                &contexts.commit(),
                vertex(7),
                DerivedPublicationStage::FoldCommittedTemplate,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, WriteError::CommittedNeedsRecovery { .. }));
        assert_eq!(
            crate::execute::write_refusal(&error).code,
            ErrorCode::OutcomeUnknown
        );
        drop(guard);
        // Neither retained session owns an active source operation, so both
        // may stay alive while the recovery worker completes.
        assert_eq!(settled(db).await, DatabaseStatus::Ready { generation: 2 });
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
        assert!(matches!(
            pinned.query(
                &contexts.query(),
                "MATCH (n) RETURN n",
                &fgdb_gql::GqlParameters::new()
            ),
            Err(crate::execute::SessionError::Unavailable(
                Unavailable::Stale
            ))
        ));
        let refusal = crate::execute::poll(&root, db, &token, &mut old)
            .await
            .err()
            .unwrap();
        assert_eq!(refusal.code, ErrorCode::DatabaseRecovering);
        assert!(!refusal.message.contains("owner"));
        let mut replacement = crate::execute::subscribe(&root, db, &token, &statement)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        old.consumer.close();
        drop(old);
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 1);
        let baseline = crate::execute::poll(&root, db, &token, &mut replacement)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message))
            .unwrap();
        assert!(baseline.batch.is_snapshot());
        assert_eq!(baseline.batch.frontier(), CommitSeq(1));
        assert_eq!(baseline.batch.rows().len(), 1);
        replacement.consumer.close();
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_authoritative_reopen_stays_fenced_after_storage_is_repaired() {
    let ((), report) = run_async_under_lab(0x79a0_0003, |root| async move {
        let (server, _, path) = served(&root, "sticky-reopen-failure").await;
        let db = &server.databases["test"];
        let admitted = db.db.enter().unwrap();
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut guard = db.db.write(&root).await.unwrap();
        let error = guard
            .write_with_publication_failure(
                &contexts.commit(),
                vertex(9),
                DerivedPublicationStage::FoldCommittedTemplate,
            )
            .await
            .unwrap_err();
        assert_eq!(
            crate::execute::write_refusal(&error).code,
            ErrorCode::OutcomeUnknown
        );
        drop(guard);
        // Only this newly created test database is damaged. The marker is
        // durable, so its capsule must authenticate; no tail truncation is an
        // acceptable replacement for an authoritative recovery failure.
        let capsules = std::fs::read_dir(path.join("capsules"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(capsules.len(), 1);
        let capsule = &capsules[0];
        let intact = std::fs::read(capsule).unwrap();
        std::fs::write(capsule, b"damaged committed test capsule").unwrap();
        drop(admitted);
        let failed = settled(db).await;
        assert!(
            matches!(&failed, DatabaseStatus::Fenced { generation: 2, reason } if !reason.is_empty())
        );
        std::fs::write(capsule, intact).unwrap();
        for _ in 0..3 {
            asupersync::runtime::yield_now().await;
            assert_eq!(db.db.status(), failed);
            assert!(db.db.shared.gate.state.read().unwrap().published.is_none());
            assert!(matches!(db.db.read(&root).await, Err(Unavailable::Fenced)));
            assert!(matches!(db.db.write(&root).await, Err(Unavailable::Fenced)));
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropped_outer_write_future_still_fences_and_recovers_its_marker() {
    let ((), report) = run_async_under_lab(0x79a0_0004, |root| async move {
        let (server, _, _) = served(&root, "dropped-write").await;
        let db = &server.databases["test"];
        let reached = std::sync::atomic::AtomicBool::new(false);
        let mut operation = Box::pin(async {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut guard = db.db.write(&root).await.unwrap();
            let prepared = guard.prepare_atomic_writes(vec![vertex(17)]).unwrap();
            let error = guard
                .commit_prepared_with_crash(
                    &contexts.commit(),
                    prepared,
                    Some(CrashPoint::AfterMarkerBeforeD2),
                )
                .await
                .unwrap_err();
            assert_eq!(
                crate::execute::write_refusal(&error).code,
                ErrorCode::OutcomeUnknown
            );
            reached.store(true, Ordering::Release);
            // Model cancellation before the outer adapter observes/handles
            // the native completion; the guard remains inside this future.
            core::future::pending::<()>().await;
            drop(guard);
        });
        poll_fn(|task| {
            assert!(operation.as_mut().poll(task).is_pending());
            if reached.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        drop(operation);
        assert_eq!(db.db.status(), DatabaseStatus::Recovering { generation: 2 });
        assert_eq!(settled(db).await, DatabaseStatus::Ready { generation: 2 });
        let guard = db.db.read(&root).await.unwrap();
        assert_eq!(guard.frontier().unwrap(), CommitSeq(1));
        assert!(guard.vertex(VId(17)).unwrap().is_some());
        drop(guard);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn published_read_admission_does_not_wait_for_a_writer_or_permanently_pin_history() {
    let ((), report) = run_async_under_lab(0x79a0_0008, |root| async move {
        let (server, token, _) = served(&root, "published-read-overlap").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut held = db.db.write(&root).await.unwrap();
        let mut admission = Box::pin(crate::execute::read_session(&root, db, &token));
        let (mut first, at) = match admission
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            Poll::Ready(Ok(result)) => result,
            Poll::Ready(Err(error)) => panic!("{}", error.message),
            Poll::Pending => panic!("a published read waited behind an unchanged writer"),
        };
        drop(admission);
        assert_eq!(at, CommitSeq(0));
        let (rows, result_at) = first
            .query_with_snapshot(
                &contexts.query(),
                "MATCH (n) RETURN 7 AS value",
                &fgdb_gql::GqlParameters::new(),
            )
            .ok()
            .unwrap();
        assert_eq!(result_at, at);
        assert!(matches!(rows, fgdb::QueryResult::Rows { rows, .. } if rows.is_empty()));
        drop(first);

        // No actual reader remains: the published record must not retain the
        // old allocation after advancement. The weak ownership in production
        // also permits make_mut to dissociate it without copying metadata.
        held.write(&contexts.commit(), vertex(1)).await.unwrap();
        assert!(
            db.db
                .shared
                .gate
                .state
                .read()
                .unwrap()
                .published
                .as_ref()
                .unwrap()
                .upgrade()
                .is_none()
        );
        let mut fallback = Box::pin(db.db.read_view(&root));
        assert!(
            fallback
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        drop(fallback);
        drop(held);

        let (old, generation) = db.db.read_view(&root).await.unwrap();
        assert_eq!(old.frontier(), CommitSeq(1));
        assert!(old.vertex(VId(1)).unwrap().is_some());
        let mut held = db.db.write(&root).await.unwrap();
        held.write(&contexts.commit(), vertex(2)).await.unwrap();
        // A real old pin keeps the published generation available throughout
        // the successor commit. The gate does not expose the successor early.
        let mut overlap = Box::pin(db.db.read_view(&root));
        let (during, during_generation) = match overlap
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            Poll::Ready(Ok(result)) => result,
            _ => panic!("the retained published generation must be immediately pinnable"),
        };
        drop(overlap);
        assert_eq!(during.frontier(), CommitSeq(1));
        assert!(during.vertex(VId(2)).unwrap().is_none());
        generation.check().unwrap();
        during_generation.check().unwrap();

        // Prepared admission and eager execution use the same generation,
        // including their result sequence, without re-entering the writer.
        let prepared = crate::execute::prepare_read(
            &root,
            db,
            &token,
            &fgdb_protocol::body::Prepare {
                mode: fgdb_protocol::body::PrepareMode::Read,
                statement: "MATCH (n) RETURN $v AS value".into(),
                parameters: vec![("v".into(), fgdb_protocol::body::WireValue::Int(9))],
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        let answer = crate::execute::read_prepared(
            &root,
            db,
            &token,
            &prepared,
            &[("v".into(), fgdb_protocol::body::WireValue::Int(11))],
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            answer.outcome,
            fgdb_protocol::body::Outcome::Rows { seq: 1 }
        );
        assert_eq!(
            answer.rows,
            vec![vec![fgdb_protocol::body::WireValue::Int(11)]]
        );
        drop(held);
        let (fresh, _) = db.db.read_view(&root).await.unwrap();
        assert_eq!(fresh.frontier(), CommitSeq(2));
        assert!(fresh.vertex(VId(2)).unwrap().is_some());
        assert_eq!(old.frontier(), CommitSeq(1));
        assert!(old.vertex(VId(2)).unwrap().is_none());
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn abort_before_worker_first_poll_fences_instead_of_leaving_a_ready_slot() {
    let ((), report) = run_async_under_lab(0x79a0_0005, |root| async move {
        let (server, _, _) = served(&root, "unpolled-worker").await;
        let db = &server.databases["test"];
        // No await follows spawn inside served(), so this root has not given
        // the worker a first poll. Its captured exit guard must still run.
        db.db.worker.try_lock().unwrap().as_ref().unwrap().abort();
        db.db.stop_and_join(&root).await;
        assert!(matches!(db.db.status(), DatabaseStatus::Fenced { .. }));
        assert!(db.db.shared.gate.state.read().unwrap().published.is_none());
        assert!(matches!(db.db.generation(), Err(Unavailable::Fenced)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unused_server_drop_releases_worker_and_read_polls_do_not_wake_subscriptions() {
    let (weak, report) = run_async_under_lab(0x79a0_0006, |root| async move {
        let (server, token, _) = served(&root, "unused-server").await;
        let db = &server.databases["test"];
        let mut watcher = db.commits.watcher();
        assert!(!watcher.poll_changed(&std::task::Context::from_waker(std::task::Waker::noop())));
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (mut session, _) = crate::execute::read_session(&root, db, &token)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        for _ in 0..3 {
            assert!(
                session
                    .query(
                        &contexts.query(),
                        "MATCH (n) RETURN n",
                        &fgdb_gql::GqlParameters::new()
                    )
                    .is_ok()
            );
            assert!(
                !watcher.poll_changed(&std::task::Context::from_waker(std::task::Waker::noop())),
                "ordinary reads must not manufacture a commit wake"
            );
        }
        let weak = Arc::downgrade(&db.db.shared);
        drop(session);
        drop(watcher);
        drop(server);
        weak
    });
    assert!(report.lab_test_passed(), "{report:?}");
    assert!(
        weak.upgrade().is_none(),
        "dropping a server without a listener must not retain its worker/database"
    );
}

#[test]
fn stop_releases_published_index_ownership_and_late_writer_cannot_restore_it() {
    let ((), report) = run_async_under_lab(0x79a0_0009, |root| async move {
        let (server, _, _) = served(&root, "published-read-stop").await;
        let db = &server.databases["test"];
        let generation = db.db.generation().unwrap();
        let mut held = db.db.write(&root).await.unwrap();
        assert!(generation.gate.state.read().unwrap().published.is_some());
        db.db.stop();
        assert!(generation.gate.state.read().unwrap().published.is_none());
        assert!(matches!(
            db.db.read_view(&root).await,
            Err(Unavailable::Stopping)
        ));
        // An already-admitted writer can finish while shutdown drains. Its
        // healthy Drop must not recreate ownership in a stopped generation.
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        held.write(&contexts.commit(), vertex(1)).await.unwrap();
        drop(held);
        assert!(generation.gate.state.read().unwrap().published.is_none());
        server.join_database_workers(&root).await;
        drop(server);
        // A template or queued completion may retain the generation fence
        // beyond the server; that fence owns no snapshot or search-index pin.
        assert!(generation.gate.state.read().unwrap().published.is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancelled_shutdown_waits_for_worker_handoff_and_drains_the_worker() {
    for expired in [false, true] {
        let clock = Arc::new(asupersync::time::VirtualClock::new());
        let timer = asupersync::time::TimerDriverHandle::with_virtual_clock(clock);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .with_timer_driver(timer)
            .build()
            .unwrap();
        let budget = if expired {
            asupersync::Budget::INFINITE.with_deadline(asupersync::types::Time::ZERO)
        } else {
            asupersync::Budget::INFINITE
        };
        let cleanup = runtime.request_cx_with_budget(budget);
        if !expired {
            cleanup.cancel_fast(asupersync::types::CancelKind::User);
        }
        assert!(cleanup.checkpoint().is_err());
        let ((), report) =
            run_async_under_lab(0x79a0_0013 + u64::from(expired), move |root| async move {
                let name = if expired {
                    "expired-shutdown-worker-handoff"
                } else {
                    "cancelled-shutdown-worker-handoff"
                };
                let (server, _, _) = served(&root, name).await;
                let db = &server.databases["test"];
                let weak = Arc::downgrade(&db.db.shared);
                let admitted = db.db.enter().unwrap();
                let mut finishing = Box::pin(server.finish_shutdown(&cleanup));
                {
                    // The handoff must wait asynchronously even after cancellation.
                    // Hold no non-Send lock or poll context over the await below.
                    let held = db.db.worker.try_lock().unwrap();
                    let wakes = Arc::new(WakeCounter::default());
                    let waker = std::task::Waker::from(Arc::clone(&wakes));
                    let mut task = std::task::Context::from_waker(&waker);
                    assert!(finishing.as_mut().poll(&mut task).is_pending());
                    assert!(held.as_ref().is_some());
                    assert!(cleanup.checkpoint().is_err(), "mask restored after Pending");
                    assert!(matches!(db.db.enter(), Err(Unavailable::Stopping)));
                    drop(held);
                    assert!(wakes.0.load(Ordering::Relaxed) > 0);
                    // Now the handle is owned by the join. Its worker must remain
                    // alive until the previously admitted operation releases it.
                    assert!(finishing.as_mut().poll(&mut task).is_pending());
                    assert!(db.db.worker.try_lock().unwrap().is_none());
                    assert!(
                        cleanup.checkpoint().is_err(),
                        "join does not mask its caller"
                    );
                }
                drop(admitted);
                finishing.await;
                assert_eq!(db.db.status(), DatabaseStatus::Stopped);
                assert!(cleanup.checkpoint().is_err());
                server.finish_shutdown(&cleanup).await;
                drop(server);
                assert!(
                    weak.upgrade().is_none(),
                    "shutdown joined the actual recovery child"
                );
            });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

fn configure_beacon(config: &mut DatabaseConfig) {
    use fgdb_beacon::read::ReadOptions;
    use fgdb_beacon::{DistanceMetric, HnswConfig};
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_gql::GraphSymbolKind;

    config
        .symbols
        .bind(GraphSymbolKind::Property, "body", 1)
        .unwrap();
    config
        .symbols
        .bind(GraphSymbolKind::Property, "x", 2)
        .unwrap();
    let mut options = ReadOptions::<PropertyKeyId, LabelId>::text(PropertyKeyId(1));
    options.projection.vector = vec![PropertyKeyId(2)];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean).exact_only());
    config
        .beacon_indexes
        .push(crate::BeaconIndexConfig::new("documents", options));
}

fn beacon_document(id: u128, text: &str) -> WriteBatch {
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::CanonicalScalar;

    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(
        VId(id),
        vec![],
        vec![
            (
                PropertyKeyId(1),
                CanonicalScalar::ucs_basic_text(text).unwrap(),
            ),
            (PropertyKeyId(2), CanonicalScalar::Int(id as i64)),
        ],
    );
    batch
}

async fn reopen_beacon(
    cx: &Cx,
    path: &std::path::Path,
    configure: impl FnOnce(&mut DatabaseConfig),
) -> Server {
    let keys = DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    );
    let mut config = DatabaseConfig::new("test", keys, AuthKey::from_seed(791));
    configure_beacon(&mut config);
    configure(&mut config);
    let mut server = Server::new(cx, ServerLimits::default()).unwrap();
    server.open_database(cx, path, config).await.unwrap();
    server
}

#[test]
fn configured_beacon_restart_restores_and_tails_without_rebuilding_the_corpus() {
    let ((), report) = run_async_under_lab(0x79a0_be03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (cold, token, path) =
            served_with_config(&root, "beacon-checkpoint-restart", |config| {
                configure_beacon(config);
                config.beacon_indexes.clear();
            })
            .await;
        let symbols = cold.databases["test"].symbols.clone();
        {
            let mut writer = cold.databases["test"].db.write(&root).await.unwrap();
            for id in 1..=16 {
                writer
                    .write(
                        &contexts.commit(),
                        beacon_document(id, if id == 1 { "needle" } else { "background" }),
                    )
                    .await
                    .unwrap();
            }
        }
        cold.finish_shutdown(&root).await;
        drop(cold);

        // The first configured startup builds and checkpoints the full corpus.
        let first = reopen_beacon(&root, &path, |_| {}).await;
        let initial = first.database_beacon_status(&root, "test").await.unwrap();
        assert_eq!(
            initial[0].restoration,
            fgdb::DerivedCheckpointOutcome::RebuiltFromChronicle {
                reason: fgdb::DerivedRebuildReason::Missing,
            }
        );
        assert_eq!(initial[0].checkpoint_sequence, Some(CommitSeq(16)));
        assert!(initial[0].checkpoint_error.is_none());
        {
            let mut writer = first.databases["test"].db.write(&root).await.unwrap();
            let mut changes = beacon_document(17, "needle");
            changes.set_vertex_property(
                VId(1),
                fgdb_delta_types::PropertyKeyId(1),
                Some(fgdb_types::CanonicalScalar::ucs_basic_text("background").unwrap()),
            );
            assert_eq!(
                writer.write(&contexts.commit(), changes).await.unwrap(),
                CommitSeq(17)
            );
        }
        first.finish_shutdown(&root).await;
        drop(first);

        // An independently opened, unregistered query supplies the answer and
        // demonstrates that the one-record allowance forbids a fresh build.
        let statement = "CALL hybrid.search(text => 'needle', text_property => 'body', \
            candidates => 1, k => 1) YIELD node RETURN node.x AS x";
        let parameters = fgdb_gql::GqlParameters::new();
        let ordinary = fgdb_gql::GqlQueryPolicy::new(100, 10, 100_000, 100_000);
        let one_record = fgdb_gql::GqlQueryPolicy::new(1, 10, 100_000, 100_000);
        let keys = DatabaseKeys::new(
            [0x91; 32],
            DatabaseSecurityNamespaceId([0x92; 32]),
            [0x93; 32],
        );
        let unregistered = Database::open(&contexts.commit(), &path, keys)
            .await
            .unwrap();
        let expected = unregistered
            .query(
                &contexts.query(),
                statement,
                &parameters,
                symbols.clone(),
                ordinary,
            )
            .unwrap();
        assert!(
            unregistered
                .query(
                    &contexts.query(),
                    statement,
                    &parameters,
                    symbols.clone(),
                    one_record
                )
                .is_err()
        );
        drop(unregistered);

        // Zero staging rows makes a fallback corpus build impossible. Restore
        // admits the persisted index and maintenance visits only two changed
        // vertices in the single committed suffix tick.
        let second = reopen_beacon(&root, &path, |config| {
            config.beacon_indexes[0].options.policy.max_staging_rows = 0;
        })
        .await;
        let restored = second.database_beacon_status(&root, "test").await.unwrap();
        assert_eq!(
            restored[0].restoration,
            fgdb::DerivedCheckpointOutcome::CatchUp {
                watermark_seq: CommitSeq(16),
                head_seq: CommitSeq(17),
                commits_replayed: 1,
                touched_vertices: 2,
                healed_by_raptorq: false,
            }
        );
        assert_eq!(restored[0].checkpoint_sequence, Some(CommitSeq(17)));
        assert!(restored[0].checkpoint_error.is_none());
        let served = &second.databases["test"];
        let (view, _) = served.db.read_view(&root).await.unwrap();
        assert_eq!(
            view.query(
                &contexts.query(),
                statement,
                &parameters,
                symbols,
                one_record
            )
            .unwrap(),
            expected
        );
        let answer = crate::execute::read(
            &root,
            served,
            &token,
            &Execute {
                mode: ExecuteMode::Read,
                statement: statement.to_owned(),
                parameters: vec![],
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(answer.columns, ["x"]);
        assert_eq!(answer.rows.len(), 1);
        second.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn configured_beacon_corruption_rebuilds_and_save_failure_keeps_serving() {
    let ((), report) = run_async_under_lab(0x79a0_be04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (initial, token, path) =
            served_with_config(&root, "beacon-checkpoint-faults", configure_beacon).await;
        let checkpoint = {
            let mut writer = initial.databases["test"].db.write(&root).await.unwrap();
            writer
                .write(&contexts.commit(), beacon_document(1, "needle"))
                .await
                .unwrap();
            path.join(writer.beacon_checkpoint_relative_path("documents"))
        };
        initial.finish_shutdown(&root).await;
        drop(initial);
        std::fs::write(&checkpoint, b"truncated derived checkpoint").unwrap();

        let rebuilt = reopen_beacon(&root, &path, |_| {}).await;
        let status = rebuilt.database_beacon_status(&root, "test").await.unwrap();
        assert_eq!(
            status[0].restoration,
            fgdb::DerivedCheckpointOutcome::RebuiltFromChronicle {
                reason: fgdb::DerivedRebuildReason::CorruptOrIncompatible,
            }
        );
        assert_eq!(status[0].checkpoint_sequence, Some(CommitSeq(1)));
        assert!(status[0].checkpoint_error.is_none());
        rebuilt.finish_shutdown(&root).await;
        drop(rebuilt);

        // A staging-path collision makes the optional cache publication fail
        // while the authoritative database and old checkpoint remain readable.
        std::fs::create_dir(checkpoint.with_extension("staging")).unwrap();
        let unchanged = reopen_beacon(&root, &path, |_| {}).await;
        let status = unchanged
            .database_beacon_status(&root, "test")
            .await
            .unwrap();
        assert_eq!(
            status[0].restoration,
            fgdb::DerivedCheckpointOutcome::CatchUp {
                watermark_seq: CommitSeq(1),
                head_seq: CommitSeq(1),
                commits_replayed: 0,
                touched_vertices: 0,
                healed_by_raptorq: false,
            }
        );
        // An unchanged, intact checkpoint is not needlessly encoded or saved.
        assert_eq!(status[0].checkpoint_sequence, Some(CommitSeq(1)));
        assert!(status[0].checkpoint_error.is_none());
        unchanged.databases["test"]
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), beacon_document(2, "background"))
            .await
            .unwrap();
        unchanged.finish_shutdown(&root).await;
        drop(unchanged);

        let available = reopen_beacon(&root, &path, |_| {}).await;
        let status = available
            .database_beacon_status(&root, "test")
            .await
            .unwrap();
        assert_eq!(
            status[0].restoration,
            fgdb::DerivedCheckpointOutcome::CatchUp {
                watermark_seq: CommitSeq(1),
                head_seq: CommitSeq(2),
                commits_replayed: 1,
                touched_vertices: 1,
                healed_by_raptorq: false,
            }
        );
        assert_eq!(status[0].checkpoint_sequence, None);
        assert!(status[0].checkpoint_error.is_some());
        assert!(status[0].status.failure.is_none());
        assert_eq!(status[0].status.source_sequence, CommitSeq(2));
        assert_eq!(
            available.database_status(&root, "test"),
            Some(DatabaseStatus::Ready { generation: 1 })
        );
        let answer = crate::execute::read(
            &root,
            &available.databases["test"],
            &token,
            &Execute {
                mode: ExecuteMode::Read,
                statement: "CALL hybrid.search(text => 'needle', text_property => 'body', \
                    k => 1) YIELD node RETURN node.x AS x"
                    .to_owned(),
                parameters: vec![],
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(answer.columns, ["x"]);
        assert_eq!(answer.rows.len(), 1);
        available.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn configured_beacon_reuses_published_pins_and_catches_up_before_recovery_admission() {
    let ((), report) = run_async_under_lab(0x79a0_be01, |root| async move {
        let (server, token, _) =
            served_with_config(&root, "beacon-recovery", configure_beacon).await;
        let (cold_server, _, _) = served_with_config(&root, "beacon-cold", |config| {
            configure_beacon(config);
            config.beacon_indexes.clear();
        })
        .await;
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = &server.databases["test"];
        let cold_db = &cold_server.databases["test"];
        let initial = server.database_beacon_status(&root, "test").await.unwrap();
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].name, "documents");
        assert_eq!(initial[0].status.source_sequence, CommitSeq(0));
        assert!(initial[0].status.failure.is_none());
        assert_eq!(
            initial[0].restoration,
            fgdb::DerivedCheckpointOutcome::RebuiltFromChronicle {
                reason: fgdb::DerivedRebuildReason::Missing,
            }
        );
        assert_eq!(initial[0].checkpoint_sequence, Some(CommitSeq(0)));
        assert!(initial[0].checkpoint_error.is_none());
        assert!(
            server
                .database_beacon_status(&root, "unknown")
                .await
                .is_none()
        );

        for served in [db, cold_db] {
            let mut writer = served.db.write(&root).await.unwrap();
            for id in 1..=8 {
                writer
                    .write(
                        &contexts.commit(),
                        beacon_document(id, if id == 1 { "needle" } else { "background" }),
                    )
                    .await
                    .unwrap();
            }
        }
        let (warm, generation) = db.db.read_view(&root).await.unwrap();
        let (cold, _) = cold_db.db.read_view(&root).await.unwrap();
        let statement = "CALL hybrid.search(text => 'needle', text_property => 'body', \
            candidates => 1, k => 1) YIELD node RETURN node.x AS x";
        let parameters = fgdb_gql::GqlParameters::new();
        let ordinary = fgdb_gql::GqlQueryPolicy::new(100, 10, 100_000, 100_000);
        let one_record = fgdb_gql::GqlQueryPolicy::new(1, 10, 100_000, 100_000);
        let expected = cold
            .query(
                &contexts.query(),
                statement,
                &parameters,
                cold_db.symbols.clone(),
                ordinary,
            )
            .unwrap();
        // Only the returned vertex is admitted. The same unregistered source
        // cannot build its eight-document corpus under a one-record allowance.
        assert_eq!(
            warm.query(
                &contexts.query(),
                statement,
                &parameters,
                db.symbols.clone(),
                one_record,
            )
            .unwrap(),
            expected,
        );
        assert!(
            cold.query(
                &contexts.query(),
                statement,
                &parameters,
                cold_db.symbols.clone(),
                one_record,
            )
            .is_err()
        );
        let request = Execute {
            mode: ExecuteMode::Read,
            statement: statement.into(),
            parameters: vec![],
        };
        let answer = crate::execute::read(&root, db, &token, &request)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(answer.columns, ["x"]);
        assert_eq!(answer.rows.len(), 1);

        {
            let mut writer = db.db.write(&root).await.unwrap();
            assert!(matches!(
                writer
                    .write_with_publication_failure(
                        &contexts.commit(),
                        beacon_document(9, "background"),
                        DerivedPublicationStage::FoldCommittedTemplate,
                    )
                    .await,
                Err(WriteError::CommittedNeedsRecovery { .. })
            ));
        }
        assert_eq!(settled(db).await, DatabaseStatus::Ready { generation: 2 });
        assert!(generation.check().is_err());
        let rebuilt = server.database_beacon_status(&root, "test").await.unwrap();
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].status.source_sequence, CommitSeq(9));
        assert_eq!(rebuilt[0].status.database_sequence, CommitSeq(9));
        assert_eq!(rebuilt[0].status.last_refresh.commits, 9);
        assert_eq!(
            rebuilt[0].restoration,
            fgdb::DerivedCheckpointOutcome::CatchUp {
                watermark_seq: CommitSeq(0),
                head_seq: CommitSeq(9),
                commits_replayed: 9,
                touched_vertices: 9,
                healed_by_raptorq: false,
            }
        );
        assert_eq!(rebuilt[0].checkpoint_sequence, Some(CommitSeq(9)));
        assert!(rebuilt[0].checkpoint_error.is_none());
        assert!(rebuilt[0].status.failure.is_none());
        let (reopened, _) = db.db.read_view(&root).await.unwrap();
        assert_eq!(
            reopened
                .query(
                    &contexts.query(),
                    statement,
                    &parameters,
                    db.symbols.clone(),
                    one_record,
                )
                .unwrap(),
            expected,
        );
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), beacon_document(10, "background"))
            .await
            .unwrap();
        let advanced = server.database_beacon_status(&root, "test").await.unwrap();
        assert_eq!(advanced[0].status.source_sequence, CommitSeq(10));
        assert_eq!(advanced[0].status.last_refresh.commits, 1);
        assert!(advanced[0].status.failure.is_none());
        server.finish_shutdown(&root).await;
        cold_server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn configured_beacon_failure_preserves_commits_and_fences_unbuildable_recovery() {
    let ((), report) = run_async_under_lab(0x79a0_be02, |root| async move {
        let (server, token, _) = served_with_config(&root, "beacon-rebuild-failure", |config| {
            configure_beacon(config);
            let index = &mut config.beacon_indexes[0];
            // The empty initial graph is buildable. Any later populated graph
            // exceeds the configured staging ceiling on an authoritative reopen.
            index.options.policy.max_staging_rows = 0;
            index.maintenance.max_work_units = 0;
        })
        .await;
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = &server.databases["test"];
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), beacon_document(1, "needle"))
            .await
            .unwrap();
        assert_eq!(
            server.database_status(&root, "test"),
            Some(DatabaseStatus::Ready { generation: 1 })
        );
        let failed = server.database_beacon_status(&root, "test").await.unwrap();
        assert_eq!(failed[0].status.source_sequence, CommitSeq(0));
        assert_eq!(failed[0].status.database_sequence, CommitSeq(1));
        assert_eq!(failed[0].status.failure.as_ref().unwrap().at, CommitSeq(1));

        let statement = Execute {
            mode: ExecuteMode::Read,
            statement: "MATCH (n) RETURN n".into(),
            parameters: vec![],
        };
        assert_eq!(
            crate::execute::read(&root, db, &token, &statement)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message))
                .rows
                .len(),
            1,
        );
        {
            let mut writer = db.db.write(&root).await.unwrap();
            assert!(matches!(
                writer
                    .write_with_publication_failure(
                        &contexts.commit(),
                        beacon_document(2, "background"),
                        DerivedPublicationStage::FoldCommittedTemplate,
                    )
                    .await,
                Err(WriteError::CommittedNeedsRecovery { .. })
            ));
        }
        assert!(matches!(
            settled(db).await,
            DatabaseStatus::Fenced { generation: 2, reason } if reason.contains("documents")
        ));
        assert!(server.database_beacon_status(&root, "test").await.is_none());
        let refusal = crate::execute::read(&root, db, &token, &statement)
            .await
            .err()
            .unwrap();
        assert_eq!(refusal.code, ErrorCode::DatabaseUnavailable);
        server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
