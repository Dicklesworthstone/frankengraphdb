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
    /// The most recent durably recorded head of this embedded graph and branch.
    /// A fresh database, or one containing exclusively older commits without
    /// explicit head updates, returns `None`. As with snapshot acquisition,
    /// an uncertain commit or incomplete publication requires recovery first.
    pub fn branch_head(&self) -> Result<Option<fgdb_types::MarkerRef>, super::ReadError> {
        self.frontier()?;
        Ok(self.coordinator.chain().head(super::GRAPH, super::BRANCH))
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
            db.write(&cx, batch(1)).await.unwrap();
            let first = db.branch_head().unwrap().unwrap();
            assert_eq!(first.commit_seq, CommitSeq(1));
            drop(db);
            let mut reopened = Database::open_with_vfs(&cx, vfs, &dir, keys())
                .await
                .unwrap();
            assert_eq!(reopened.branch_head().unwrap(), Some(first));
            reopened.write(&cx, batch(2)).await.unwrap();
            let second = reopened.branch_head().unwrap().unwrap();
            assert_eq!(second.commit_seq, CommitSeq(2));
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
}
