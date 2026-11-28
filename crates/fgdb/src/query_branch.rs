//! Host-resolved branch reads over the native immutable-view executor.
//!
//! A selector does not manufacture branch authority. The host resolves the
//! exact name to a view it has already admitted, and supplies the corresponding
//! symbol resolver. No branch miss or source error falls back to the default.

use super::{Database, EmbeddedReadView, QueryError, QueryResult};
use asupersync::fs::Vfs;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbolResolver, PreparedGraphBranchText};
use fgdb_types::QueryCx;

impl EmbeddedReadView {
    /// Execute an optionally branch-qualified native read.
    ///
    /// `resolve_branch` is called exactly once when the statement contains
    /// `AT BRANCH`, and never for an unqualified statement. It must resolve the
    /// exact, case-sensitive name to an authorized immutable generation; an
    /// unknown or unauthorized name must return an error. The supplied symbol
    /// resolver must describe that generation's catalog.
    ///
    /// The selected view owns every operand of a compound read and is retained
    /// for the entire execution. Temporal selectors choose history within that
    /// view, never within the default view or a live writer. Branch-only
    /// parameters are consumed by routing; shared and unknown arguments remain
    /// subject to native schema validation. Values are never interpolated.
    ///
    /// This method is read-only. It does not create, fork, merge, or write a
    /// branch, install a persistent branch catalog, or grant additional access.
    /// Ordinary `query` deliberately continues to refuse unresolved selectors.
    pub fn query_with_branch_resolver(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        resolve_branch: impl FnOnce(&str) -> Result<EmbeddedReadView, QueryError>,
        policy: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        query_selected_view(
            || Ok(self.clone()),
            cx,
            text,
            params,
            resolver,
            resolve_branch,
            policy,
        )
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Discover the current write precondition from the published manifest
    /// and verified marker chain. This metadata-only open takes no writer
    /// lease, touches no graph or capsule payload, and does not repair a
    /// lagging or missing publication. A legacy global-only tail is distinct
    /// from an empty database and cannot authorize an `empty` write guard.
    pub async fn open_current_branch_head_with_vfs(
        cx: &fgdb_types::CommitCx,
        vfs: V,
        path: impl AsRef<std::path::Path>,
        keys: crate::DatabaseKeys,
    ) -> Result<crate::PublishedBranchHead, crate::BranchRevisionOpenError> {
        use crate::{BranchRevisionOpenError as Error, PublishedBranchHead, ReadGeneration};
        let path = path.as_ref();
        crate::require_database_dir(cx, &vfs, path)
            .await
            .map_err(Error::Open)?;
        let (generation, recorded) =
            Self::read_current_generation_inner(cx, &vfs, path, &keys, None, None)
                .await
                .map_err(Error::Open)?;
        match generation {
            ReadGeneration::Unselected => Err(Error::Unpublished { published: None }),
            ReadGeneration::Lagging(root) => Err(Error::Unpublished {
                published: Some(root.published_at),
            }),
            ReadGeneration::Current(root) => Ok(match recorded {
                Some(marker) => PublishedBranchHead::Recorded(marker),
                None if root.published_at == fgdb_types::CommitSeq::ORIGIN => {
                    PublishedBranchHead::Empty
                }
                None => PublishedBranchHead::Unrecorded {
                    frontier: root.published_at,
                },
            }),
        }
    }

    /// Open a retained revision from an immutable published generation while
    /// another handle may own the writer. This path never takes a lease or
    /// repairs the database. It binds the graph root and the exact recorded
    /// coordinate update to one verified log scan. An unknown marker returns
    /// `None`; a verified marker beyond the selected publication is an error.
    pub async fn open_branch_revision_with_vfs(
        cx: &fgdb_types::CommitCx,
        vfs: V,
        path: impl AsRef<std::path::Path>,
        keys: crate::DatabaseKeys,
        marker: fgdb_types::MarkerRef,
    ) -> Result<Option<crate::BranchRevision>, crate::BranchRevisionOpenError> {
        use crate::{BranchRevisionOpenError as Error, ReadGeneration};
        let path = path.as_ref();
        crate::require_database_dir(cx, &vfs, path)
            .await
            .map_err(Error::Open)?;
        let (generation, recorded) =
            Self::read_current_generation_inner(cx, &vfs, path, &keys, None, Some(marker.commit_seq))
                .await
                .map_err(Error::Open)?;
        let published = match generation {
            ReadGeneration::Current(root) | ReadGeneration::Lagging(root) => root,
            ReadGeneration::Unselected => return Err(Error::Unpublished { published: None }),
        };
        if recorded != Some(marker) {
            return Ok(None);
        }
        if published.published_at < marker.commit_seq {
            return Err(Error::Unpublished {
                published: Some(published.published_at),
            });
        }
        let view = Self::read_published(cx, &keys, published)
            .await
            .map_err(Error::Open)?;
        crate::BranchRevision::from_view(view, marker)
            .map(Some)
            .map_err(Error::Catalog)
    }

    /// The explicit branch head of the published embedded graph. A fresh
    /// database or a frontier written without a head update returns `None`.
    /// This also covers an older writer appending after a tracked generation.
    /// An uncertain commit or incomplete publication requires recovery first.
    pub fn branch_head(&self) -> Result<Option<fgdb_types::MarkerRef>, crate::ReadError> {
        self.ensure_readable()?;
        Ok(self.published_branch_head())
    }

    fn published_branch_head(&self) -> Option<fgdb_types::MarkerRef> {
        self.coordinator
            .chain()
            .head(crate::GRAPH, crate::BRANCH)
            .filter(|marker| marker.commit_seq == self.snapshot.frontier)
    }

    /// Pin the current recorded head and its graph state under one read of
    /// this healthy handle. No marker is returned for an untracked history.
    pub fn current_branch_revision(
        &self,
    ) -> Result<Option<crate::BranchRevision>, crate::SymbolCatalogError> {
        match self.branch_head()? {
            Some(marker) => self.read_branch_revision(marker),
            None => Ok(None),
        }
    }

    /// Pin a historical revision of the embedded branch by its full marker
    /// identity. A marker outside this coordinate's verified history yields
    /// `None`. A fenced database or unavailable historical schema is an error.
    pub fn read_branch_revision(
        &self,
        marker: fgdb_types::MarkerRef,
    ) -> Result<Option<crate::BranchRevision>, crate::SymbolCatalogError> {
        self.ensure_readable()?;
        if self
            .coordinator
            .chain()
            .branch_history_from(crate::GRAPH, crate::BRANCH, marker)
            .is_none()
        {
            return Ok(None);
        }
        let view = self.read_session()?;
        crate::BranchRevision::from_view(view, marker).map(Some)
    }

    /// Check a write precondition on the current embedded branch.
    /// A caller composing this check with a separate write must retain
    /// exclusive access to the same writer until that write completes.
    pub fn check_branch_head(
        &self,
        expected: Option<fgdb_types::MarkerRef>,
    ) -> Result<(), crate::WriteError> {
        self.ensure_writable()?;
        let actual = self.published_branch_head();
        if actual.is_none() && self.snapshot.frontier != fgdb_types::CommitSeq::ORIGIN {
            return Err(crate::WriteError::BranchHeadUnrecorded {
                frontier: self.snapshot.frontier,
            });
        }
        fgdb_chronicle::HeadUpdate {
            graph: crate::GRAPH,
            branch: crate::BRANCH,
            expected_previous: expected,
        }
        .check_previous(actual)
        .map_err(fgdb_chronicle::CommitError::from)?;
        Ok(())
    }

    /// Commit only if the embedded branch is still at the supplied revision.
    ///
    /// `Some` compares both the marker OID and commit sequence. `None`
    /// requires an empty history. A nonempty legacy database first needs an
    /// ordinary write to establish its explicit branch head. Check the head
    /// before evaluating the batch so a stale writer consumes no sequence
    /// and observes no new graph data through batch validation.
    ///
    /// The exclusive handle and Chronicle writer lease span the comparison,
    /// ordinary write validation and durable marker CAS. All normal write,
    /// cancellation and recovery outcomes are preserved.
    pub async fn write_if_branch_head(
        &mut self,
        cx: &fgdb_types::CommitCx,
        expected: Option<fgdb_types::MarkerRef>,
        batch: crate::WriteBatch,
    ) -> Result<fgdb_types::CommitSeq, crate::WriteError> {
        self.check_branch_head(expected)?;
        self.write(cx, batch).await
    }

    /// Execute a native read using an explicit host branch resolver.
    ///
    /// Unqualified statements acquire a healthy read session from this
    /// database. Qualified statements instead use exactly the immutable view
    /// returned by `resolve_branch`; they do not acquire a default generation
    /// or retry there after a refusal. See
    /// [`EmbeddedReadView::query_with_branch_resolver`] for the resolver's
    /// authorization/catalog obligations and statement-wide selector semantics.
    pub fn query_with_branch_resolver(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        resolve_branch: impl FnOnce(&str) -> Result<EmbeddedReadView, QueryError>,
        policy: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        query_selected_view(
            || self.read_session().map_err(QueryError::Read),
            cx,
            text,
            params,
            resolver,
            resolve_branch,
            policy,
        )
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Database<asupersync::fs::UnixVfs> {
    /// Discover the exact published write basis while another process can
    /// own the writer. See [`Self::open_current_branch_head_with_vfs`].
    pub async fn open_current_branch_head(
        cx: &fgdb_types::CommitCx,
        path: impl AsRef<std::path::Path>,
        keys: crate::DatabaseKeys,
    ) -> Result<crate::PublishedBranchHead, crate::BranchRevisionOpenError> {
        Self::open_current_branch_head_with_vfs(cx, asupersync::fs::UnixVfs::new(), path, keys)
            .await
    }

    /// Open the exact recorded branch revision without acquiring the writer.
    /// See [`Self::open_branch_revision_with_vfs`] for selection and errors.
    pub async fn open_branch_revision(
        cx: &fgdb_types::CommitCx,
        path: impl AsRef<std::path::Path>,
        keys: crate::DatabaseKeys,
        marker: fgdb_types::MarkerRef,
    ) -> Result<Option<crate::BranchRevision>, crate::BranchRevisionOpenError> {
        Self::open_branch_revision_with_vfs(cx, asupersync::fs::UnixVfs::new(), path, keys, marker)
            .await
    }
}

impl crate::BranchRevision {
    fn from_view(
        view: EmbeddedReadView,
        marker: fgdb_types::MarkerRef,
    ) -> Result<Self, crate::SymbolCatalogError> {
        let catalog = view.symbol_catalog_at(marker.commit_seq)?;
        Ok(Self {
            marker,
            view,
            catalog,
        })
    }

    /// The exact revision used for reads and conditional writes.
    pub fn marker(&self) -> fgdb_types::MarkerRef {
        self.marker
    }

    /// Immutable name bindings visible at this revision.
    pub fn symbol_catalog(&self) -> &fgdb_delta_types::catalog::SymbolCatalog {
        &self.catalog
    }

    /// Evaluate a native graph read with this marker as its single data and
    /// catalog cut. The host supplies the ordinary query budget and context.
    /// Explicit temporal selectors and procedure calls require separate cut
    /// semantics and refuse before execution. This API carries the embedded
    /// owner's read authority and does not issue a capability or durable lease.
    pub fn query(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        self.query_with_resolver(
            cx,
            text,
            params,
            fgdb_gql::symbol_bindings::CatalogSymbolResolver::new(&self.catalog),
            policy,
        )
    }

    /// Run a read with host-supplied statement name bindings and pinned time
    /// zones. Start from [`Self::symbol_catalog`] when extending the symbol
    /// namespace. The data cut remains this revision for every query operand.
    pub fn query_with_resolver(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        policy: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        use crate::PreparedNativeRead;

        let lowered = fgdb_gql::GqlMapArgumentText::lower_fields(text, params);
        let (text, params) = lowered
            .as_ref()
            .map_or((text, params), |(text, params)| (text.text(), params));
        let prepared = PreparedNativeRead::prepare(text, params, resolver)
            .map_err(|error| error.original_text(0, lowered.as_ref().map(|(text, _)| text)))?;
        prepared.check_branch_revision_read()?;
        prepared.execute_in_view_at_seq(&self.view, cx, params, policy, self.marker.commit_seq)
    }
}

impl crate::PreparedNativeRead {
    /// Admit a statement for execution at one externally authenticated branch
    /// revision. Temporal selectors or procedures could select a different or
    /// unproved source. The caller still binds every physical source to the
    /// marker's sequence before reading.
    pub fn check_branch_revision_read(&self) -> Result<(), QueryError> {
        if self.has_procedure_call() {
            return Err(QueryError::Unsupported {
                diagnostics: vec!["procedure sources are unavailable on recorded branch revisions".into()],
            });
        }
        match self {
            Self::TemporalPattern(_)
            | Self::TemporalSet(_)
            | Self::TemporalAggregate(_) => Err(QueryError::Unsupported {
                diagnostics: vec!["the recorded branch revision already selects an exact snapshot".into()],
            }),
            Self::Pattern(_)
            | Self::Aggregate(_)
            | Self::PipelineAggregate(_)
            | Self::Set(_) => Ok(()),
        }
    }
}

fn query_selected_view(
    default_view: impl FnOnce() -> Result<EmbeddedReadView, QueryError>,
    cx: &QueryCx,
    text: &str,
    params: &GqlParameters,
    resolver: impl GraphSymbolResolver,
    resolve_branch: impl FnOnce(&str) -> Result<EmbeddedReadView, QueryError>,
    policy: GqlQueryPolicy,
) -> Result<QueryResult, QueryError> {
    // Selector errors contain only structural classes and original offsets.
    // Native graph syntax, binding, budgets and source errors retain their
    // existing variants. Nothing here executes a parse-shaped graph statement.
    let selector_error = |error: fgdb_gql::GraphBranchTextError| QueryError::Unsupported {
        diagnostics: vec![error.to_string()],
    };
    let prepared = PreparedGraphBranchText::prepare(text).map_err(selector_error)?;
    let bound = prepared.bind_parameters(params).map_err(selector_error)?;
    let view = match bound.branch() {
        Some(name) => resolve_branch(name)?,
        None => default_view()?,
    };
    view.query(cx, bound.statement(), bound.parameters(), resolver, policy)
}

#[cfg(test)]
mod head_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::RelationId;
    use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

    #[test]
    fn embedded_writes_link_the_recovered_head() {
        let ((), report) = run_async_under_lab(0xb4a1_0001, |runtime| async move {
            let cx = PurposeContexts::narrow_runtime_root(&runtime).commit();
            let vfs = MemVfs::new().unwrap();
            let dir = vfs.database_dir();
            let keys = || DatabaseKeys::new([3; 32], DatabaseSecurityNamespaceId([4; 32]), [5; 32]);
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &dir, keys())
                .await
                .unwrap();
            assert_eq!(db.branch_head().unwrap(), None);
            assert_eq!(
                Database::open_current_branch_head_with_vfs(&cx, vfs.clone(), &dir, keys()).await.unwrap(),
                crate::PublishedBranchHead::Empty,
            );
            let batch = |id| {
                let mut batch = WriteBatch::new(RelationId(1));
                batch.create_vertex(VId(id), vec![], vec![]);
                batch
            };
            db.write_if_branch_head(&cx, None, batch(1)).await.unwrap();
            let first = db.branch_head().unwrap().unwrap();
            assert_eq!(first.commit_seq, CommitSeq(1));
            assert_eq!(
                Database::open_current_branch_head_with_vfs(&cx, vfs.clone(), &dir, keys()).await.unwrap(),
                crate::PublishedBranchHead::Recorded(first),
            );
            drop(db);
            let mut reopened = Database::open_with_vfs(&cx, vfs, &dir, keys())
                .await
                .unwrap();
            assert_eq!(reopened.branch_head().unwrap(), Some(first));
            for expected in [
                None,
                Some(fgdb_types::MarkerRef {
                    marker_oid: fgdb_types::ObjectId([0xaa; 32]),
                    ..first
                }),
            ] {
                let result = reopened
                    .write_if_branch_head(&cx, expected, WriteBatch::new(RelationId(1)))
                    .await;
                assert!(matches!(result,
                    Err(crate::WriteError::Commit(error))
                    if matches!(*error, fgdb_chronicle::CommitError::Chain(
                        fgdb_chronicle::ChainError::HeadCasMismatch(ref e)
                    ) if e.actual == Some(first) && e.expected == expected)
                ));
                assert_eq!(reopened.branch_head().unwrap(), Some(first));
            }
            reopened
                .write_if_branch_head(&cx, Some(first), batch(2))
                .await
                .unwrap();
            let second = reopened.branch_head().unwrap().unwrap();
            assert_eq!(second.commit_seq, CommitSeq(2));
            assert!(
                reopened
                    .write_if_branch_head(&cx, Some(first), batch(3))
                    .await
                    .is_err()
            );
            assert_eq!(reopened.branch_head().unwrap(), Some(second));
            assert!(
                reopened
                    .vertex_at(VId(3), second.commit_seq)
                    .unwrap()
                    .is_none()
            );
            let actual: Vec<_> = reopened
                .coordinator
                .chain()
                .branch_history_from(crate::GRAPH, crate::BRANCH, second)
                .unwrap()
                .map(|entry| entry.marker_oid)
                .collect();
            assert_eq!(actual, vec![second.marker_oid, first.marker_oid]);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn a_markerless_tail_cannot_satisfy_any_head_condition() {
        let ((), report) = run_async_under_lab(0xb4a1_0002, |runtime| async move {
            let cx = PurposeContexts::narrow_runtime_root(&runtime).commit();
            for tracked_prefix in [false, true] {
                let vfs = MemVfs::new().unwrap();
                let dir = vfs.database_dir();
                let keys = || DatabaseKeys::new([3; 32], DatabaseSecurityNamespaceId([4; 32]), [5; 32]);
                let mut db = Database::create_with_vfs(&cx, vfs.clone(), &dir, keys())
                    .await
                    .unwrap();
                let previous = if tracked_prefix {
                    let mut tracked = WriteBatch::new(RelationId(1));
                    tracked.create_vertex(VId(10), vec![], vec![]);
                    db.write(&cx, tracked).await.unwrap();
                    db.branch_head().unwrap()
                } else {
                    None
                };
                let mut legacy = WriteBatch::new(RelationId(1));
                legacy.create_vertex(VId(1), vec![], vec![]);
                let template = db.build_write_template(legacy).unwrap();
                let capsule =
                    crate::prepare_capsule(db.keys.k_oid(), db.keys.namespace, &template).unwrap();
                let old = db
                    .coordinator
                    .commit(&cx, &capsule.bytes, |seq, oid| {
                        crate::marker_for_capsule(seq, oid, &capsule, vec![])
                    })
                    .await
                    .unwrap();
                // A global-only marker is not a recorded branch update,
                // even when its complete chain digest was supplied.
                assert!(Database::open_branch_revision_with_vfs(
                    &cx, vfs.clone(), &dir, keys(), old,
                ).await.unwrap().is_none());
                drop(db);
                let mut reopened = Database::open_with_vfs(&cx, vfs.clone(), &dir, keys())
                    .await
                    .unwrap();
                assert_eq!(reopened.branch_head().unwrap(), None);
                assert_eq!(
                    Database::open_current_branch_head_with_vfs(&cx, vfs.clone(), &dir, keys()).await.unwrap(),
                    crate::PublishedBranchHead::Unrecorded { frontier: old.commit_seq },
                );
                assert!(
                    reopened
                        .vertex_at(VId(1), old.commit_seq)
                        .unwrap()
                        .is_some()
                );
                for expected in [None, Some(old), previous] {
                    assert!(matches!(
                        reopened.write_if_branch_head(&cx, expected, WriteBatch::new(RelationId(1))).await,
                        Err(crate::WriteError::BranchHeadUnrecorded { frontier }) if frontier == old.commit_seq
                    ));
                }
                let mut first_tracked = WriteBatch::new(RelationId(1));
                first_tracked.create_vertex(VId(2), vec![], vec![]);
                reopened.write(&cx, first_tracked).await.unwrap();
                assert_eq!(
                    reopened.branch_head().unwrap().unwrap().commit_seq,
                    old.commit_seq.checked_successor().unwrap()
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn committed_but_unpublished_revision_does_not_replace_the_reader_cut() {
        let ((), report) = run_async_under_lab(0xb4a1_0004, |runtime| async move {
            let cx = PurposeContexts::narrow_runtime_root(&runtime).commit();
            let vfs = MemVfs::new().unwrap();
            let dir = vfs.database_dir();
            let keys = || DatabaseKeys::new([3; 32], DatabaseSecurityNamespaceId([4; 32]), [5; 32]);
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &dir, keys())
                .await
                .unwrap();
            let mut first = WriteBatch::new(RelationId(1));
            first.create_vertex(VId(1), vec![], vec![]);
            db.write(&cx, first).await.unwrap();
            let published = db.branch_head().unwrap().unwrap();
            let mut second = WriteBatch::new(RelationId(1));
            second.create_vertex(VId(2), vec![], vec![]);
            let template = db.build_write_template(second).unwrap();
            let capsule =
                crate::prepare_capsule(db.keys.k_oid(), db.keys.namespace, &template).unwrap();
            let head = fgdb_chronicle::HeadUpdate {
                graph: crate::GRAPH,
                branch: crate::BRANCH,
                expected_previous: Some(published),
            };
            let pending = db
                .coordinator
                .commit(&cx, &capsule.bytes, |seq, oid| {
                    crate::marker_for_capsule(seq, oid, &capsule, vec![head])
                })
                .await
                .unwrap();
            assert!(matches!(
                Database::open_current_branch_head_with_vfs(&cx, vfs.clone(), &dir, keys()).await,
                Err(crate::BranchRevisionOpenError::Unpublished { published: Some(at) }) if at == published.commit_seq
            ));
            let read =
                Database::open_branch_revision_with_vfs(&cx, vfs.clone(), &dir, keys(), pending)
                    .await;
            assert!(
                matches!(read, Err(crate::BranchRevisionOpenError::Unpublished { published: Some(at) }) if at == published.commit_seq)
            );
            let older =
                Database::open_branch_revision_with_vfs(&cx, vfs.clone(), &dir, keys(), published)
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(older.marker(), published);
            let (scan, on_sibling) =
                fgdb_chronicle::CommitCoordinator::scan_committed_branch_prefix(
                    &cx,
                    &vfs,
                    &dir,
                    pending.commit_seq,
                    crate::GRAPH,
                    fgdb_types::BranchId(2),
                    published,
                )
                .await
                .unwrap();
            assert_eq!(scan.frontier(), pending.commit_seq);
            assert!(!on_sibling);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn revision_queries_pin_rows_catalog_and_write_basis_at_one_marker() {
        use fgdb_delta_types::LabelId;
        use fgdb_delta_types::catalog::{CatalogSymbol, CatalogSymbolKind, SymbolCatalog};

        let ((), report) = run_async_under_lab(0xb4a1_0003, |runtime| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&runtime);
            let cx = contexts.commit();
            let query = contexts.query();
            let vfs = MemVfs::new().unwrap();
            let dir = vfs.database_dir();
            let keys = || DatabaseKeys::new([3; 32], DatabaseSecurityNamespaceId([4; 32]), [5; 32]);
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &dir, keys()).await.unwrap();
            let mut names = SymbolCatalog::default();
            names.bind("Early", CatalogSymbol::Label(LabelId(1))).unwrap();
            db.register_symbols(&cx, &names).await.unwrap();
            let mut one = WriteBatch::new(RelationId(1));
            one.create_vertex(VId(1), vec![LabelId(1)], vec![]);
            db.write(&cx, one).await.unwrap();
            let original = db.current_branch_revision().unwrap().unwrap();
            let mut later_names = SymbolCatalog::default();
            later_names.bind("Later", CatalogSymbol::Label(LabelId(2))).unwrap();
            db.register_symbols(&cx, &later_names).await.unwrap();
            let mut two = WriteBatch::new(RelationId(1));
            two.create_vertex(VId(2), vec![LabelId(1)], vec![]);
            db.write(&cx, two).await.unwrap();
            let newest = db.current_branch_revision().unwrap().unwrap();
            // Reopen immutable storage while this writer still owns its lease.
            let readonly = Database::open_branch_revision_with_vfs(
                &cx, vfs.clone(), &dir, keys(), original.marker(),
            ).await.unwrap().unwrap();
            assert!(Database::open_branch_revision_with_vfs(
                &cx, vfs.clone(), &dir, keys(), fgdb_types::MarkerRef {
                    marker_oid: newest.marker().marker_oid, ..original.marker()
                },
            ).await.unwrap().is_none());
            drop(db);
            let reopened = Database::open_with_vfs(&cx, vfs, &dir, keys()).await.unwrap();
            let historical = reopened.read_branch_revision(original.marker()).unwrap().unwrap();
            assert!(reopened.read_branch_revision(fgdb_types::MarkerRef {
                marker_oid: newest.marker().marker_oid, ..original.marker()
            }).unwrap().is_none());
            drop(reopened);
            let params = GqlParameters::new();
            let policy = GqlQueryPolicy::new(100, 100, 10000, 10000);
            for (revision, count) in [(&original, 1), (&historical, 1), (&readonly, 1), (&newest, 2)] {
                let result = revision.query(&query, "MATCH (v:Early) RETURN v", &params, policy).unwrap();
                assert!(matches!(result, QueryResult::Rows { rows, .. } if rows.len() == count));
            }
            assert!(readonly.symbol_catalog().resolve(CatalogSymbolKind::Label, "Later").is_none());
            assert!(historical.symbol_catalog().resolve(CatalogSymbolKind::Label, "Later").is_none());
            assert!(historical.query(&query, "MATCH (v:Later) RETURN v", &params, policy).is_err());
            assert!(newest.query(&query, "MATCH (v:Later) RETURN v", &params, policy).is_ok());
            let compound = "MATCH (v:Early) RETURN v AS x UNION ALL MATCH (w:Early) RETURN w AS x";
            assert!(matches!(
                historical.query(&query, compound, &params, policy).unwrap(),
                QueryResult::Rows { rows, .. } if rows.len() == 2
            ));
            for temporal in [
                "MATCH (v) FOR SYSTEM_TIME AS OF SEQ 4 RETURN v",
                "MATCH (v) FOR SYSTEM_TIME AS OF SEQ 4 RETURN v AS x UNION ALL MATCH (w) RETURN w AS x",
                "MATCH (v) RETURN v AS x UNION ALL MATCH (w) FOR SYSTEM_TIME AS OF SEQ 4 RETURN w AS x",
                "CALL { MATCH (v) FOR SYSTEM_TIME AS OF SEQ 4 RETURN v AS x } RETURN x",
            ] {
                assert!(historical.query(&query, temporal, &params, policy).is_err(), "{temporal}");
            }
            for value in [17, 23] {
                let map = GqlParameters::new()
                    .with_map("m", vec![("value".into(), fgdb_gql::algebra::GraphValue::Scalar(fgdb_types::CanonicalScalar::Int(value)))])
                    .unwrap();
                assert_eq!(
                    historical.query(&query, "MATCH (v:Early) RETURN $m.value AS result", &map, policy).unwrap(),
                    historical.query(&query, &format!("MATCH (v:Early) RETURN {value} AS result"), &params, policy).unwrap(),
                );
            }
            assert!(historical.query(&query, "MATCH (v) RETURN v", &params, GqlQueryPolicy::new(0, 0, 0, 0)).is_err());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
