//! Native result differences over one already-authenticated embedded history.
//!
//! Bind once, evaluate the complete query at both exact cuts through existing
//! engines, then consolidate signed native rows. No first-leaf substitution,
//! repeated parsing, event-log replay, caller-supplied coverage assertion, or
//! separately mutable graph image participates in this operation.

use super::{Cancel, PreparedNativeRead, QueryError};
use crate::{Database, EmbeddedReadView};
use asupersync::fs::Vfs;
use fgdb_gql::result_diff::{DiffEndpoint, GraphDiffError, GraphDiffInput, GraphResultDiff};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbolResolver};
use fgdb_types::{CommitSeq, QueryCx};

type Error = GqlQueryError<GraphDiffError<QueryError>, Cancel>;

fn definition(error: QueryError) -> Error {
    GqlQueryError::Source(GraphDiffError::Definition(error))
}
fn admit(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    before: CommitSeq,
    after: CommitSeq,
) -> Result<(), Error> {
    // Both endpoint fences precede resolver/binding callbacks or source work,
    // including equal cuts and LIMIT 0. Existing engines also enforce every
    // history/source precondition; none is bypassed to produce an empty bag.
    for endpoint in [DiffEndpoint::Before, DiffEndpoint::After] {
        view.snapshot
            .check_frontier(endpoint.sequence(before, after))
            .map_err(|error| {
                GqlQueryError::Source(GraphDiffError::Endpoint {
                    endpoint,
                    source: QueryError::Read(error),
                })
            })?;
    }
    cx.with_restriction(|| cx.checkpoint())
        .map_err(GqlQueryError::Interrupted)
}

impl<V: Vfs + Clone> Database<V> {
    /// Exact NET result changes between two revisions of this opened history.
    /// Prepare and bind one native query, evaluate its complete semantics at
    /// `before` and `after`, and return after-minus-before as compressed signed
    /// rows. A row present twice before and once after has weight -1, not zero.
    /// A changed property/aggregate retracts the old complete row and inserts
    /// the new row. Pure order changes are not bag changes; query DISTINCT,
    /// ORDER BY, OFFSET/LIMIT and all child scopes still run at each endpoint.
    ///
    /// Both evaluations use one immutable read generation, so a resolver
    /// cannot cause mixed cuts. Explicit temporal statements refuse instead
    /// of overriding either endpoint. Reverse endpoints are legal. Equal cuts
    /// still validate and execute the query; they cannot hide a query error.
    /// Counts, wide sums, exact averages, NULLs and graph identities retain
    /// their original domains, including textual RETURN slot order.
    ///
    /// One policy covers the SUM of source records, work and scratch across
    /// both queries and exact consolidation. ResultRows bounds consolidated
    /// changed tuples, not endpoint rows or absolute signed occurrences. A
    /// refused endpoint or exhausted budget returns no partial difference.
    /// Use PreparedNativeRead::diff to reuse frozen parsing/name resolution.
    ///
    /// This is the same-history, complete-endpoint result-difference subset:
    /// not reserved DIFF syntax, cross-branch comparison, intermediate events,
    /// a CDC cursor, partial-coverage proof, durable lease or spill execution.
    /// It needs retained endpoint state, not an intervening retained delta log.
    #[allow(clippy::too_many_arguments)]
    pub fn query_diff(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<GraphResultDiff, Error> {
        // Fenced live handles cannot mint a new admitted generation.
        let view = self
            .read_session()
            .map_err(|error| definition(QueryError::Read(error)))?;
        view.query_diff(cx, text, params, resolver, before, after, policy)
    }
}

impl EmbeddedReadView {
    /// Compare two exact revisions retained by THIS immutable generation.
    /// A later writer or reopened handle is never consulted. Neither endpoint
    /// may exceed this view's frontier, even if the live writer has advanced.
    /// The same cumulative budget and all-or-nothing difference apply as for
    /// Database::query_diff. Returned values do not borrow the view or query.
    #[allow(clippy::too_many_arguments)]
    pub fn query_diff(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<GraphResultDiff, Error> {
        admit(self, cx, before, after)?;
        cx.with_restriction(|| {
            let prepared =
                PreparedNativeRead::prepare(text, params, resolver).map_err(definition)?;
            prepared.diff_admitted(self, cx, params, before, after, policy)
        })
    }
}

impl PreparedNativeRead {
    /// Compare one frozen template at two committed revisions. Parameter
    /// binding happens once per operation, not once per endpoint. The entire
    /// bound pattern, aggregate/WITH input or set tree executes at each cut.
    /// No temporal-selector stripping or physical fallback is performed.
    #[allow(clippy::too_many_arguments)]
    pub fn diff<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &QueryCx,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<GraphResultDiff, Error> {
        let view = database
            .read_session()
            .map_err(|error| definition(QueryError::Read(error)))?;
        self.diff_in_view(&view, cx, params, before, after, policy)
    }

    /// Reuse a pinned history without acquiring the writer's latest generation.
    /// Source errors identify Before/After; budget and interruption errors
    /// remain the ordinary typed GQL errors rather than becoming empty input.
    #[allow(clippy::too_many_arguments)]
    pub fn diff_in_view(
        &self,
        view: &EmbeddedReadView,
        cx: &QueryCx,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<GraphResultDiff, Error> {
        admit(view, cx, before, after)?;
        cx.with_restriction(|| self.diff_admitted(view, cx, params, before, after, policy))
    }

    fn diff_admitted(
        &self,
        view: &EmbeddedReadView,
        cx: &QueryCx,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<GraphResultDiff, Error> {
        // Keep each native source class and bind error. A complete aggregate's
        // private input is NOT replaced by its primary pattern; compound and
        // computed stages are owned by the existing aggregate/set executor.
        match self {
            Self::Pattern(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(|error| definition(QueryError::PatternText(error)))?;
                GraphResultDiff::execute(
                    before,
                    after,
                    query.columns().to_vec(),
                    policy,
                    |endpoint, remaining| {
                        view.execute_graph_pattern_governed_at(
                            cx,
                            &query,
                            endpoint.sequence(before, after),
                            remaining,
                        )
                        .map(GraphDiffInput::values)
                        .map_err(|error| {
                            error.map_source(|source| {
                                QueryError::Pattern(GqlQueryError::Source(source))
                            })
                        })
                    },
                    || cx.checkpoint(),
                )
            }
            Self::Aggregate(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(|error| definition(QueryError::PatternText(error)))?;
                GraphResultDiff::execute(
                    before,
                    after,
                    prepared.columns().to_vec(),
                    policy,
                    |endpoint, remaining| {
                        view.execute_graph_aggregate_governed_at(
                            cx,
                            &query,
                            endpoint.sequence(before, after),
                            remaining,
                        )
                        .map(|result| {
                            GraphDiffInput::aggregates(
                                result,
                                prepared.output_slots(),
                                query.key_columns().len(),
                                query.aggregate_columns().len(),
                            )
                        })
                        .map_err(|error| {
                            error.map_source(|source| {
                                QueryError::Aggregate(GqlQueryError::Source(source))
                            })
                        })
                    },
                    || cx.checkpoint(),
                )
            }
            Self::PipelineAggregate(prepared) => {
                // A WITH pipeline can have zero or several graph sources.
                // Choose its source-aware owner from the frozen definition,
                // just as ordinary reads do. Falling back to the first leaf
                // would discard joins, null extension and input-local pages.
                if prepared.requires_relational_input() {
                    let query = prepared
                        .bind_relation_parameters(params)
                        .map_err(|error| definition(QueryError::PipelineText(error)))?;
                    return GraphResultDiff::execute(
                        before,
                        after,
                        prepared.columns().to_vec(),
                        policy,
                        |endpoint, remaining| {
                            view.execute_graph_set_aggregate_governed_at(
                                cx,
                                &query,
                                endpoint.sequence(before, after),
                                remaining,
                            )
                            .map(|result| {
                                GraphDiffInput::aggregates(
                                    result,
                                    prepared.output_slots(),
                                    query.key_columns().len(),
                                    query.aggregate_columns().len(),
                                )
                            })
                            .map_err(|error| {
                                error.map_source(|source| {
                                    QueryError::Aggregate(GqlQueryError::Source(source))
                                })
                            })
                        },
                        || cx.checkpoint(),
                    );
                }
                let query = prepared
                    .bind_parameters(params)
                    .map_err(|error| definition(QueryError::PipelineText(error)))?;
                GraphResultDiff::execute(
                    before,
                    after,
                    prepared.columns().to_vec(),
                    policy,
                    |endpoint, remaining| {
                        view.execute_graph_aggregate_governed_at(
                            cx,
                            &query,
                            endpoint.sequence(before, after),
                            remaining,
                        )
                        .map(|result| {
                            GraphDiffInput::aggregates(
                                result,
                                prepared.output_slots(),
                                query.key_columns().len(),
                                query.aggregate_columns().len(),
                            )
                        })
                        .map_err(|error| {
                            error.map_source(|source| {
                                QueryError::Aggregate(GqlQueryError::Source(source))
                            })
                        })
                    },
                    || cx.checkpoint(),
                )
            }
            Self::Set(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(|error| definition(QueryError::SetText(error)))?;
                GraphResultDiff::execute(
                    before,
                    after,
                    prepared.columns().to_vec(),
                    policy,
                    |endpoint, remaining| {
                        view.execute_graph_set_governed_at(
                            cx,
                            &query,
                            endpoint.sequence(before, after),
                            remaining,
                        )
                        .map(GraphDiffInput::values)
                        .map_err(|error| {
                            error
                                .map_source(|source| QueryError::Set(GqlQueryError::Source(source)))
                        })
                    },
                    || cx.checkpoint(),
                )
            }
            Self::TemporalPattern(_) | Self::TemporalAggregate(_) | Self::TemporalSet(_) => {
                Err(GqlQueryError::Source(GraphDiffError::TemporalSelector))
            }
        }
    }
}

#[cfg(test)]
mod relational_input_tests {
    use super::*;
    use crate::{DatabaseKeys, QueryResult, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GraphAggregateValue, RelationBind};
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
    use std::collections::BTreeMap;

    type Bag = BTreeMap<Vec<GraphAggregateValue>, i128>;
    const P: PropertyKeyId = PropertyKeyId(1);
    const R: RelationId = RelationId(1);
    const JOINED: &str = "MATCH (a) WITH a.p AS x MATCH (b) WITH x, b.p AS y \
        RETURN COUNT(*) AS n, SUM(y) AS total, AVG(y) AS average";

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0x58; 32],
            DatabaseSecurityNamespaceId([0x59; 32]),
            [0x5a; 32],
        )
    }
    fn symbols() -> RelationBind {
        RelationBind::new()
            .with_property("p", P)
            .with_relation("R", R)
    }
    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
    }
    fn seed() -> WriteBatch {
        let mut batch = WriteBatch::new(R);
        for (id, value) in [(1, 1), (2, 3)] {
            batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
        }
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch
    }
    fn extension() -> WriteBatch {
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(3), vec![], vec![(P, CanonicalScalar::Int(5))]);
        batch.add_edge(EId(11), VId(1), VId(3), vec![]);
        batch
    }
    fn prepare(text: &str, params: &GqlParameters) -> PreparedNativeRead {
        let prepared = PreparedNativeRead::prepare(text, params, symbols()).unwrap();
        assert!(
            matches!(
                &prepared,
                PreparedNativeRead::PipelineAggregate(query) if query.requires_relational_input()
            ),
            "the regression must exercise source-aware pipeline binding: {text}"
        );
        prepared
    }
    fn bag(result: QueryResult) -> Bag {
        let QueryResult::Rows { rows, .. } = result else {
            panic!("read rows required");
        };
        let mut result = Bag::new();
        for row in rows {
            *result.entry(row).or_default() += 1;
        }
        result
    }
    fn observed(result: &GraphResultDiff) -> Bag {
        result
            .changes()
            .iter()
            .map(|(row, weight)| (row.to_vec(), weight.to_i128().unwrap()))
            .collect()
    }
    fn subtract(before: &Bag, after: &Bag) -> Bag {
        // Independent integer counts, not the production Z-set difference.
        let mut result = after.clone();
        for (row, weight) in before {
            *result.entry(row.clone()).or_default() -= weight;
        }
        result.retain(|_, weight| *weight != 0);
        result
    }

    #[test]
    fn all_relational_endpoints_match_complete_reads_not_a_primary_leaf() {
        let ((), report) = run_async_under_lab(0xd1ff_2101, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let empty = db.read_session().unwrap();
            db.write(&commit, seed()).await.unwrap();
            let before = db.read_session().unwrap();
            db.write(&commit, extension()).await.unwrap();
            let after = db.read_session().unwrap();
            let params = GqlParameters::new();
            let views = [&empty, &before, &after];
            for text in [
                JOINED,
                // A carried vertex's property is projected before OPTIONAL
                // MATCH, the spelling the pipeline grammar admits.
                "MATCH (a) WITH a AS a, a.p AS ap OPTIONAL MATCH (a)-[:R]->(b) \
                 WITH ap AS p, b.p AS q \
                 RETURN p AS first, COUNT(q) AS n, p AS again GROUP BY p ORDER BY n DESC, first",
                "MATCH (a) WITH a.p AS x ORDER BY x DESC LIMIT 1 MATCH (b) \
                 WITH x, b.p AS y WHERE y < x RETURN SUM(y) AS total, COUNT(*) AS n",
                "UNWIND [1, 2, 2] AS x WITH x RETURN COUNT(*) AS n, SUM(x) AS total",
                "UNWIND [] AS x WITH x RETURN COUNT(*) AS n",
            ] {
                let prepared = prepare(text, &params);
                let answers: Vec<_> = views
                    .iter()
                    .map(|view| {
                        bag(prepared
                            .execute_in_view(view, &cx, &params, policy())
                            .unwrap())
                    })
                    .collect();
                for (a, before) in views.iter().enumerate() {
                    for (b, after) in views.iter().enumerate() {
                        let result = prepared
                            .diff_in_view(
                                views[2],
                                &cx,
                                &params,
                                before.frontier(),
                                after.frontier(),
                                policy(),
                            )
                            .unwrap();
                        assert_eq!(
                            observed(&result),
                            subtract(&answers[a], &answers[b]),
                            "{text}"
                        );
                    }
                }
            }
            let result = db
                .query_diff(
                    &cx,
                    JOINED,
                    &params,
                    symbols(),
                    before.frontier(),
                    after.frontier(),
                    policy(),
                )
                .unwrap();
            // Two-by-two and three-by-three Cartesian bags. A first-leaf
            // substitute returns counts 2/3 and sums 4/9 instead of 4/9, 8/27.
            let summary = |n, total, denominator| {
                vec![
                    GraphAggregateValue::Count(n),
                    GraphAggregateValue::Integer(total),
                    GraphAggregateValue::Average(
                        fgdb_gql::GraphExactAverage::new(total, denominator).unwrap(),
                    ),
                ]
            };
            assert_eq!(
                observed(&result),
                Bag::from([(summary(4, 8, 4), -1), (summary(9, 27, 9), 1)])
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn relational_diff_rebinds_once_and_pinned_history_outlives_the_writer() {
        let ((), report) = run_async_under_lab(0xd1ff_2102, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.write(&commit, seed()).await.unwrap();
            let after = db.write(&commit, extension()).await.unwrap();
            let view = db.read_session().unwrap();
            let text = "MATCH (a) WITH a.p AS x MATCH (b) WITH x, b.p AS y \
                        WHERE y >= $floor RETURN COUNT(*) AS n, SUM(y) AS total";
            let low = GqlParameters::new().with_int64("floor", 0).unwrap();
            let prepared = prepare(text, &low);
            let low_result = prepared
                .diff(&db, &cx, &low, before, after, policy())
                .unwrap();
            let high = GqlParameters::new().with_int64("floor", 4).unwrap();
            let high_result = prepared
                .diff(&db, &cx, &high, before, after, policy())
                .unwrap();
            assert_ne!(observed(&low_result), observed(&high_result));
            let mut edit = WriteBatch::new(R);
            edit.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(99)));
            db.write(&commit, edit).await.unwrap();
            drop(db);
            assert_eq!(
                prepared
                    .diff_in_view(&view, &cx, &low, before, after, policy())
                    .unwrap(),
                low_result
            );
            assert_eq!(
                prepared
                    .diff_in_view(&view, &cx, &high, before, after, policy())
                    .unwrap(),
                high_result
            );
            assert!(matches!(
                prepared.diff_in_view(&view, &cx, &GqlParameters::new(), before, after, policy()),
                Err(GqlQueryError::Source(GraphDiffError::Definition(
                    QueryError::PipelineText(_)
                )))
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn late_input_errors_survive_equal_cuts_reverse_order_and_limit_zero() {
        let ((), report) = run_async_under_lab(0xd1ff_2103, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.write(&commit, seed()).await.unwrap();
            let mut edit = WriteBatch::new(R);
            edit.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(0)));
            let after = db.write(&commit, edit).await.unwrap();
            let params = GqlParameters::new();
            for suffix in ["", " LIMIT 0"] {
                let text = format!(
                    "MATCH (a) WITH a.p AS x MATCH (b) WITH x, b.p AS y \
                     RETURN SUM(8 / y) AS total{suffix}"
                );
                let prepared = prepare(&text, &params);
                assert!(
                    prepared
                        .diff(&db, &cx, &params, before, before, policy())
                        .unwrap()
                        .changes()
                        .is_empty()
                );
                for (a, b, expected) in [
                    (before, after, DiffEndpoint::After),
                    (after, before, DiffEndpoint::Before),
                    (after, after, DiffEndpoint::Before),
                ] {
                    assert!(matches!(
                        prepared.diff(&db, &cx, &params, a, b, policy()),
                        Err(GqlQueryError::Source(GraphDiffError::Endpoint {
                            endpoint,
                            source: QueryError::Aggregate(_),
                        })) if endpoint == expected
                    ));
                }
            }
            // Even a source-free, equal-cut, zero-result statement executes
            // its input expressions; no fabricated empty endpoint is legal.
            let bad = prepare(
                "UNWIND [0] AS x WITH x RETURN SUM(1 / x) AS total LIMIT 0",
                &params,
            );
            assert!(matches!(
                bad.diff(&db, &cx, &params, before, before, policy()),
                Err(GqlQueryError::Source(GraphDiffError::Endpoint {
                    endpoint: DiffEndpoint::Before,
                    source: QueryError::Aggregate(_),
                }))
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn joined_sources_and_consolidation_share_the_original_allowance() {
        let ((), report) = run_async_under_lab(0xd1ff_2104, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.write(&commit, seed()).await.unwrap();
            let after = db.write(&commit, extension()).await.unwrap();
            let params = GqlParameters::new();
            let prepared = prepare(JOINED, &params);
            let result = prepared
                .diff(&db, &cx, &params, before, after, policy())
                .unwrap();
            let rows = result.row_stats();
            let work = result.evaluator_stats();
            assert_eq!(rows.result_rows, 2);
            assert!(rows.snapshot_records >= 10);
            let exact = GqlQueryPolicy::new(
                rows.snapshot_records,
                2,
                work.work_units,
                work.scratch_entries,
            );
            assert_eq!(
                prepared
                    .diff(&db, &cx, &params, before, after, exact)
                    .unwrap(),
                result
            );
            for smaller in [
                GqlQueryPolicy::new(
                    rows.snapshot_records - 1,
                    2,
                    work.work_units,
                    work.scratch_entries,
                ),
                GqlQueryPolicy::new(
                    rows.snapshot_records,
                    1,
                    work.work_units,
                    work.scratch_entries,
                ),
                GqlQueryPolicy::new(
                    rows.snapshot_records,
                    2,
                    work.work_units - 1,
                    work.scratch_entries,
                ),
                GqlQueryPolicy::new(
                    rows.snapshot_records,
                    2,
                    work.work_units,
                    work.scratch_entries - 1,
                ),
            ] {
                assert!(matches!(
                    prepared.diff(&db, &cx, &params, before, after, smaller),
                    Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))
                ));
                assert_eq!(
                    prepared
                        .diff(&db, &cx, &params, before, after, exact)
                        .unwrap(),
                    result
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
