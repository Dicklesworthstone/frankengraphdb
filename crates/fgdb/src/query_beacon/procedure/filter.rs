//! Bounded, query-private endpoint eligibility for CALL hybrid.search.
//! This is a result restriction, never a source/authority or transit filter.

use std::cell::RefCell;
use std::collections::BTreeSet;

use fgdb_beacon::{BeaconError, WorkControl};
use fgdb_gql::algebra::GraphValue;
use fgdb_types::{CanonicalScalar, VId};

pub(crate) struct EndpointFilter {
    ids: Option<BTreeSet<VId>>,
    failure: RefCell<Option<BeaconError>>,
}

impl EndpointFilter {
    pub(crate) fn prepare(
        values: Option<&[GraphValue]>,
        result_capacity: usize,
        scratch_limit: u64,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        Self::prepare_with_admission(values, result_capacity, scratch_limit, work, || Ok(()))
    }

    /// Admit each retained ID to the enclosing scratch meter BEFORE insertion.
    /// Input/work limits still count duplicate occurrences; retained scratch
    /// counts each identity once. A refusal returns no partially built filter.
    /// Immutable hosts retain their existing accounting through `prepare`.
    pub(crate) fn prepare_with_admission(
        values: Option<&[GraphValue]>,
        result_capacity: usize,
        scratch_limit: u64,
        work: &mut dyn WorkControl,
        mut admit_entry: impl FnMut() -> Result<(), BeaconError>,
    ) -> Result<Self, BeaconError> {
        let Some(values) = values else {
            // An omitted/null restriction preserves the existing no-filter
            // charge trace. In particular, it is not an empty allowed set.
            return Ok(Self {
                ids: None,
                failure: RefCell::new(None),
            });
        };
        work.charge(1)?;
        // Bound input occurrences, not just unique IDs: duplicates must not
        // defeat ingress admission. Reserve the requested result capacity too,
        // before allocating this private set or starting source access.
        if values.len() as u128 + result_capacity as u128 > u128::from(scratch_limit) {
            return Err(BeaconError::ResourceLimit {
                resource: "eligible vertex scratch entries",
                limit: usize::try_from(scratch_limit).unwrap_or(usize::MAX),
            });
        }
        let mut ids = BTreeSet::new();
        for value in values {
            work.charge(2 * (1 + ids.len().checked_ilog2().unwrap_or(0) as usize))?;
            let id = match value {
                GraphValue::Vertex(id) => *id,
                GraphValue::Scalar(CanonicalScalar::Int(id)) if *id >= 0 => VId(*id as u128),
                _ => {
                    return Err(BeaconError::InvalidQuery(
                        "eligible must contain vertices or nonnegative integer IDs",
                    ));
                }
            };
            if !ids.contains(&id) {
                admit_entry()?;
                ids.insert(id);
            }
        }
        Ok(Self {
            ids: Some(ids),
            failure: RefCell::new(None),
        })
    }

    pub(crate) fn scratch_entries(&self) -> u64 {
        self.ids.as_ref().map_or(0, |ids| ids.len() as u64)
    }

    /// The native predicate ABI returns bool. Retain a failed work/authority
    /// check separately so it can NEVER be mistaken for ordinary ineligibility.
    /// The caller must finish the entire retrieval through `finish` below.
    pub(crate) fn admits(&self, id: VId, work: &RefCell<impl WorkControl>) -> bool {
        if self.failure.borrow().is_some() {
            return false;
        }
        let Some(ids) = &self.ids else {
            return true;
        };
        let units = 1 + ids.len().checked_ilog2().unwrap_or(0) as usize;
        match work.borrow_mut().charge(units) {
            Ok(()) => ids.contains(&id),
            Err(error) => {
                *self.failure.borrow_mut() = Some(error);
                false
            }
        }
    }

    /// No successful prefix or later error may replace the first refusal in
    /// the bool predicate. The host meter separately retains typed expiry and
    /// cancellation causes and maps them after this private result is discarded.
    pub(crate) fn finish<T>(&self, result: Result<T, BeaconError>) -> Result<T, BeaconError> {
        match self.failure.borrow().as_ref() {
            Some(error) => Err(error.clone()),
            None => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_beacon::WorkBudget;

    #[test]
    fn omitted_empty_duplicate_and_full_width_id_sets_have_distinct_semantics() {
        let work = RefCell::new(WorkBudget::new(10_000));
        let all = EndpointFilter::prepare(None, usize::MAX, 0, &mut *work.borrow_mut()).unwrap();
        assert!(all.admits(VId(u128::MAX), &work));
        let empty = EndpointFilter::prepare(Some(&[]), 1, 1, &mut *work.borrow_mut()).unwrap();
        assert!(!empty.admits(VId(0), &work));
        let values = [
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Scalar(CanonicalScalar::Int(0)),
            GraphValue::Vertex(VId(u128::MAX)),
        ];
        let selected =
            EndpointFilter::prepare(Some(&values), 1, 4, &mut *work.borrow_mut()).unwrap();
        assert_eq!(selected.scratch_entries(), 2);
        assert!(selected.admits(VId(0), &work));
        assert!(selected.admits(VId(u128::MAX), &work));
        assert!(!selected.admits(VId(1), &work));
        assert!(selected.finish(Ok(())).is_ok());
    }

    #[test]
    fn input_and_result_capacity_are_admitted_before_set_allocation() {
        let values = [GraphValue::Vertex(VId(1)), GraphValue::Vertex(VId(1))];
        assert!(matches!(
            EndpointFilter::prepare(Some(&values), 1, 2, &mut WorkBudget::new(100)),
            Err(BeaconError::ResourceLimit {
                resource: "eligible vertex scratch entries",
                limit: 2
            })
        ));
        for value in [
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Bool(true)),
            GraphValue::Scalar(CanonicalScalar::Int(-1)),
        ] {
            assert!(matches!(
                EndpointFilter::prepare(Some(&[value]), 1, 2, &mut WorkBudget::new(100)),
                Err(BeaconError::InvalidQuery(_))
            ));
        }
    }

    #[test]
    fn lookup_refusal_cannot_be_returned_as_an_empty_success_or_replaced() {
        let filter = EndpointFilter::prepare(
            Some(&[GraphValue::Vertex(VId(1))]),
            1,
            2,
            &mut WorkBudget::new(100),
        )
        .unwrap();
        let work = RefCell::new(WorkBudget::new(0));
        assert!(!filter.admits(VId(1), &work));
        assert!(matches!(
            filter.finish(Ok(Vec::<VId>::new())),
            Err(BeaconError::WorkBudgetExceeded)
        ));
        assert!(matches!(
            filter.finish::<()>(Err(BeaconError::InvalidQuery("later refusal"))),
            Err(BeaconError::WorkBudgetExceeded)
        ));
        let replenished = RefCell::new(WorkBudget::new(100));
        assert!(!filter.admits(VId(1), &replenished));
        assert!(matches!(
            filter.finish(Ok(())),
            Err(BeaconError::WorkBudgetExceeded)
        ));
    }

    #[test]
    fn every_preparation_and_lookup_checkpoint_can_cancel_without_a_result() {
        struct Cut {
            count: usize,
            stop: usize,
        }
        impl WorkControl for Cut {
            fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
                self.count += 1;
                if self.count == self.stop {
                    Err(BeaconError::Cancelled)
                } else {
                    Ok(())
                }
            }
        }
        let run = |stop| {
            let work = RefCell::new(Cut { count: 0, stop });
            let result = (|| {
                let values = [GraphValue::Vertex(VId(1)), GraphValue::Vertex(VId(2))];
                let filter = EndpointFilter::prepare(Some(&values), 1, 3, &mut *work.borrow_mut())?;
                let accepted = filter.admits(VId(1), &work);
                filter.finish(Ok(accepted))
            })();
            (result, work.into_inner().count)
        };
        let (result, count) = run(usize::MAX);
        assert!(result.unwrap());
        for stop in 1..=count {
            let (result, seen) = run(stop);
            assert!(matches!(result, Err(BeaconError::Cancelled)), "cut {stop}");
            assert_eq!(seen, stop);
        }
    }

    #[test]
    fn retained_admissions_are_unique_and_fail_before_later_input_work() {
        use std::cell::Cell;
        struct Work<'a> {
            admitted: &'a Cell<usize>,
            trace: Vec<usize>,
        }
        impl WorkControl for Work<'_> {
            fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
                self.trace.push(self.admitted.get());
                Ok(())
            }
        }
        let values = [1, 1, 2, 3].map(|id| GraphValue::Vertex(VId(id)));
        let admitted = Cell::new(0);
        let mut work = Work {
            admitted: &admitted,
            trace: Vec::new(),
        };
        let result = EndpointFilter::prepare_with_admission(Some(&values), 1, 5, &mut work, || {
            admitted.set(admitted.get() + 1);
            if admitted.get() == 2 {
                Err(BeaconError::ResourceLimit {
                    resource: "test retained IDs",
                    limit: 1,
                })
            } else {
                Ok(())
            }
        });
        assert!(matches!(
            result,
            Err(BeaconError::ResourceLimit {
                resource: "test retained IDs",
                limit: 1
            })
        ));
        assert_eq!(admitted.get(), 2);
        assert_eq!(work.trace, [0, 0, 1, 1]);
    }

    #[test]
    fn absent_empty_and_duplicate_filters_share_the_retained_admission_law() {
        let mut admissions = 0;
        for values in [None, Some(&[][..])] {
            let filter = EndpointFilter::prepare_with_admission(
                values,
                0,
                0,
                &mut WorkBudget::new(100),
                || {
                    admissions += 1;
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(filter.scratch_entries(), 0);
        }
        assert_eq!(admissions, 0);
        let values = [1, 1, 2, 1].map(|id| GraphValue::Vertex(VId(id)));
        let filter = EndpointFilter::prepare_with_admission(
            Some(&values),
            1,
            5,
            &mut WorkBudget::new(100),
            || {
                admissions += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(admissions, 2);
        assert_eq!(filter.scratch_entries(), 2);
    }
}

#[cfg(test)]
mod native_tests {
    use crate::{Database, DatabaseKeys, QueryResult, QueryValue, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use asupersync::security::key::AuthKey;
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
    use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

    const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xa2; 32]);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    }

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(10_000, 1000, 10_000_000, 100_000)
    }

    fn statement(eligible: &str) -> String {
        format!(
            "CALL hybrid.search(text => 'graph', text_property => 'body', \
             vector => [0], vector_properties => ['x'], eligible => {eligible}, \
             seeds => [1], max_hops => 2, candidates => 1, graph_candidates => 1, \
             graph_weight => 100, k => 1) \
             YIELD node, graph_hops, vector_rank, text_rank \
             RETURN node.x AS x, graph_hops, vector_rank, text_rank"
        )
    }

    fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
        match result {
            QueryResult::Rows { rows, .. } => rows,
            QueryResult::Write { .. } => panic!("read procedure returned a write"),
        }
    }

    fn expected() -> Vec<Vec<QueryValue>> {
        vec![
            [3, 2, 1, 1]
                .into_iter()
                .map(|n| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n))))
                .collect(),
        ]
    }

    #[test]
    fn native_calls_filter_before_all_candidate_limits_and_preserve_authorized_transit() {
        let ((), report) = run_async_under_lab(0xbeac_3401, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db =
                Database::open_memory(&commit, DatabaseKeys::new([0xa1; 32], NS, [0xa3; 32]))
                    .await
                    .unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            for id in [1, 2, 3, 90] {
                batch.create_vertex(
                    VId(id),
                    vec![LabelId(if id == 90 { 99 } else { 1 })],
                    vec![
                        (
                            PropertyKeyId(1),
                            CanonicalScalar::ucs_basic_text("graph").unwrap(),
                        ),
                        (PropertyKeyId(2), CanonicalScalar::Int(id as i64)),
                    ],
                );
            }
            for (id, a, b) in [(1, 1, 2), (2, 2, 3), (3, 1, 90), (4, 90, 3)] {
                batch.add_edge(EId(id), VId(a), VId(b), vec![]);
            }
            db.write(&commit, batch).await.unwrap();
            let params = GqlParameters::new();
            for allowed in ["[3]", "[3, 3, 999]"] {
                assert_eq!(
                    rows(
                        db.query(&cx, &statement(allowed), &params, symbols, policy())
                            .unwrap()
                    ),
                    expected()
                );
            }
            assert!(
                rows(
                    db.query(&cx, &statement("[]"), &params, symbols, policy())
                        .unwrap()
                )
                .is_empty()
            );
            assert!(
                db.query(&cx, &statement("[-1]"), &params, symbols, policy())
                    .is_err()
            );
            let unrestricted = rows(
                db.query(&cx, &statement("null"), &params, symbols, policy())
                    .unwrap(),
            );
            assert_eq!(unrestricted.len(), 1);
            assert_eq!(
                unrestricted[0][0],
                QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2)))
            );

            let authority = Authority::new(
                AuthKey::from_seed(3401),
                NS,
                "hybrid-filter-host",
                SchemaEpoch(0),
                1,
            )
            .unwrap();
            let mut grant = Grant::read_only(
                "main",
                1000,
                QueryLimits {
                    max_nodes: 100,
                    max_work: 1_000_000,
                    max_rows: 10,
                },
            );
            grant.labels = Scope::only([LabelId(1)]);
            grant.relations = Scope::only([RelationId(1)]);
            grant.properties = Scope::only([PropertyKeyId(1), PropertyKeyId(2)]);
            let token = authority.issue_at(&grant, 100).unwrap();
            let mut session = db
                .authorized_read_session(&cx, &authority, &token, "main", symbols, policy(), || 100)
                .unwrap();
            // The hidden supplied ID cannot win, route, or unmask any property.
            let prepared = session
                .prepare(&cx, &statement("[90, 3]"), &params)
                .unwrap();
            assert_eq!(
                rows(session.execute(&cx, &prepared, &params).unwrap()),
                expected()
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
