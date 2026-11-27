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
            let batch = |id| {
                let mut batch = WriteBatch::new(RelationId(1));
                batch.create_vertex(VId(id), vec![], vec![]);
                batch
            };
            db.write_if_branch_head(&cx, None, batch(1)).await.unwrap();
            let first = db.branch_head().unwrap().unwrap();
            assert_eq!(first.commit_seq, CommitSeq(1));
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
                drop(db);
                let mut reopened = Database::open_with_vfs(&cx, vfs, &dir, keys())
                    .await
                    .unwrap();
                assert_eq!(reopened.branch_head().unwrap(), None);
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
}
