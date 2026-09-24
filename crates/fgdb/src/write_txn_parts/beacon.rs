//! Read-your-own-writes Beacon retrieval. This is a bounded, per-read derived
//! index over the transaction's pinned history and canonical prepared net,
//! never a durable index, a second graph authority, or an implicit commit.

use super::{WriteTxn, WriteTxnError};
use crate::gql_exec::source::{self, SourceEvent};
use crate::{Database, PendingRow, VertexRow};
use asupersync::fs::Vfs;
use fgdb_beacon::read::{ReadError, ReadOptions, Rows, Search};
use fgdb_beacon::{BeaconError, BeaconIndex, WorkBudget, WorkControl};
use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId};
use fgdb_types::{CanonicalScalar, QueryCx, VId};
use std::cell::RefCell;
use std::collections::{BTreeMap, btree_map::Entry};

type Options = ReadOptions<PropertyKeyId, LabelId>;
type Cancel = Box<asupersync::error::Error>;

/// All source metadata admissions, including new read witnesses, use the
/// same conservative per-call scratch counter. It counts admissions, not RSS.
struct Control<'a> {
    cx: &'a QueryCx,
    budget: WorkBudget,
    scratch: usize,
    limit: usize,
    interrupted: Option<Cancel>,
}

impl WorkControl for Control<'_> {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        if self.interrupted.is_some() {
            return Err(BeaconError::Cancelled);
        }
        if let Err(error) = self.cx.checkpoint() {
            self.interrupted = Some(error);
            return Err(BeaconError::Cancelled);
        }
        self.budget.charge(units)
    }
}

impl Control<'_> {
    fn source(&mut self, event: SourceEvent) -> Result<(), BeaconError> {
        self.charge(1)?;
        if matches!(event, SourceEvent::ScratchEntry) {
            if self.scratch == self.limit {
                return Err(BeaconError::ResourceLimit {
                    resource: "transaction search source scratch entries",
                    limit: self.limit,
                });
            }
            self.scratch += 1;
        }
        Ok(())
    }
}

struct Shared<'a, 'q>(&'a RefCell<Control<'q>>);
impl WorkControl for Shared<'_, '_> {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        self.0.borrow_mut().charge(units)
    }
}

/// Borrow payloads until a selected document is projected under Beacon's
/// byte/coordinate limits. Do not clone entire vertex rows before admission.
#[derive(Default)]
struct Overlay<'a> {
    basis: Option<&'a VertexRow>,
    created: Option<(&'a [LabelId], &'a [(PropertyKeyId, CanonicalScalar)])>,
    deleted: bool,
    labels: BTreeMap<LabelId, bool>,
    properties: BTreeMap<PropertyKeyId, Option<&'a CanonicalScalar>>,
}

impl<'a> Overlay<'a> {
    fn contents(&self) -> Option<(&'a [LabelId], &'a [(PropertyKeyId, CanonicalScalar)])> {
        if self.deleted {
            None
        } else {
            self.created.or_else(|| {
                self.basis
                    .map(|row| (row.labels.as_slice(), row.props.as_slice()))
            })
        }
    }

    fn apply(&mut self, row: &'a DeltaRow, work: &RefCell<Control<'_>>) -> Result<(), BeaconError> {
        match row {
            DeltaRow::CreateVertex { labels, props, .. } => {
                self.created = Some((labels, props));
                self.deleted = false;
                self.labels.clear();
                self.properties.clear();
            }
            DeltaRow::DeleteVertex { .. } => {
                self.deleted = true;
                self.created = None;
                self.labels.clear();
                self.properties.clear();
            }
            DeltaRow::LabelMembership { label, after, .. } if !self.deleted => {
                if !self.labels.contains_key(label) {
                    work.borrow_mut().source(SourceEvent::ScratchEntry)?;
                }
                self.labels.insert(*label, *after);
            }
            DeltaRow::Property { property, after, .. } if !self.deleted => {
                if !self.properties.contains_key(property) {
                    work.borrow_mut().source(SourceEvent::ScratchEntry)?;
                }
                self.properties.insert(*property, after.as_ref());
            }
            _ => {}
        }
        Ok(())
    }
}

fn candidate<'a, 'm>(
    rows: &'m mut BTreeMap<VId, Overlay<'a>>,
    vid: VId,
    work: &RefCell<Control<'_>>,
) -> Result<&'m mut Overlay<'a>, BeaconError> {
    match rows.entry(vid) {
        Entry::Occupied(entry) => Ok(entry.into_mut()),
        Entry::Vacant(entry) => {
            work.borrow_mut().source(SourceEvent::ScratchEntry)?;
            Ok(entry.insert(Overlay::default()))
        }
    }
}

impl WriteTxn {
    /// Search this transaction's pinned snapshot plus its canonical staged
    /// writes, without preparing or publishing another write. Text, vector
    /// and exact-rational hybrid fusion use the same Beacon engines as the
    /// committed read API. `as_of` must be absent or equal to `self.basis()`.
    ///
    /// This is a privileged embedded API, not a capability-scoped entrypoint.
    /// The entire corpus is observed, not only top-k hits: BM25 statistics and
    /// ranking depend on omitted rows too. Label-scoped scans retain label
    /// insertion witnesses; unscoped scans retain all-vertex witnesses.
    /// Failed reads retain observations already made, including normalized
    /// away identities. Savepoint rollback does not erase those observations.
    ///
    /// Construction is per-call and resident, with source metadata, staging,
    /// projection and search limits. This is not a maintained index or an
    /// external-memory claim. Vector exactness and ANN semantics are unchanged.
    pub fn beacon_search<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &QueryCx,
        options: &Options,
        query: Search<'_>,
    ) -> Result<Rows, ReadError<WriteTxnError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(ReadError::Interrupted)?;
            self.ensure_database(database).map_err(ReadError::Read)?;
            if options.as_of.is_some_and(|at| at != self.basis) {
                return Err(ReadError::Index(BeaconError::InvalidQuery(
                    "transaction search must use the pinned transaction basis",
                )));
            }
            let view = database
                .read_session()
                .map_err(|error| ReadError::Read(error.into()))?;
            view.snapshot
                .check_frontier(self.basis)
                .map_err(|error| ReadError::Read(error.into()))?;
            let work = RefCell::new(Control {
                cx,
                budget: WorkBudget::new(options.policy.max_work_units),
                scratch: 0,
                limit: options.policy.max_source_scratch,
                interrupted: None,
            });
            let result = (|| {
                work.borrow_mut().charge(1)?;
                let config = options.config_for(query)?;
                query.validate(&config, &mut Shared(&work))?;
                if let Some(label) = options.vertex_label {
                    if !self.scanned_vertex_labels.borrow().contains(&label) {
                        work.borrow_mut().source(SourceEvent::ScratchEntry)?;
                        self.scanned_vertex_labels.borrow_mut().insert(label);
                    }
                } else {
                    self.scanned_vertices.set(true);
                }
                let observe = |vid| -> Result<(), BeaconError> {
                    work.borrow_mut().charge(1)?;
                    let element = ElementId::Vertex(vid);
                    if !self.read_set.borrow().contains(&element) {
                        work.borrow_mut().source(SourceEvent::ScratchEntry)?;
                        self.read_set.borrow_mut().insert(element);
                    }
                    Ok(())
                };
                // The prepared net can erase create/delete pairs and no-op
                // ensures. Their absent identities still carry observations.
                for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
                    work.borrow_mut().charge(1)?;
                    if let PendingRow::Vertex { vid, .. } | PendingRow::DeleteVertex { vid, .. } =
                        pending
                    {
                        observe(*vid)?;
                    }
                }
                let mut rows = BTreeMap::new();
                if let Some(prepared) = &self.prepared {
                    for coordinate in prepared.template.coordinate_entries() {
                        work.borrow_mut().charge(1)?;
                        for effect in &coordinate.rows {
                            work.borrow_mut().charge(1)?;
                            let vid = match effect {
                                DeltaRow::CreateVertex { vid, .. }
                                | DeltaRow::DeleteVertex { vid, .. }
                                | DeltaRow::LabelMembership { vid, .. }
                                | DeltaRow::Property {
                                    elem: ElementId::Vertex(vid), ..
                                } => *vid,
                                _ => continue,
                            };
                            observe(vid)?;
                            candidate(&mut rows, vid, &work)?.apply(effect, &work)?;
                        }
                    }
                }
                // Select actual historical winners before label/property
                // projection, using the same governed source visitor as GLA.
                source::visit_vertices(
                    &view.snapshot.patches,
                    self.basis,
                    &mut |event| work.borrow_mut().source(event),
                    |row, control| {
                        control(SourceEvent::Work)?;
                        observe(row.vid)?;
                        candidate(&mut rows, row.vid, &work)?.basis = Some(row);
                        Ok(())
                    },
                )?;
                let mut staged = 0usize;
                let projected = rows.into_iter().filter_map(|(vid, overlay)| {
                    (|| {
                        work.borrow_mut().charge(1)?;
                        let Some((labels, props)) = overlay.contents() else {
                            return Ok(None);
                        };
                        work.borrow_mut().charge(labels.len())?;
                        if options.vertex_label.is_some_and(|label| {
                            !overlay.labels.get(&label).copied()
                                .unwrap_or_else(|| labels.binary_search(&label).is_ok())
                        }) {
                            return Ok(None);
                        }
                        let limit = options.policy.max_staging_rows.min(config.max_documents);
                        if staged == limit {
                            return Err(BeaconError::ResourceLimit {
                                resource: "staged vertices", limit,
                            });
                        }
                        staged += 1;
                        options.projection.project(
                            vid,
                            &config,
                            |key| {
                                overlay.properties.get(&key).copied().unwrap_or_else(|| {
                                    props.binary_search_by_key(&key, |(key, _)| *key)
                                        .ok().map(|slot| &props[slot].1)
                                })
                            },
                            &mut Shared(&work),
                        ).map(Some)
                    })().transpose()
                });
                let index = BeaconIndex::try_build(config.clone(), projected, &mut Shared(&work))?;
                let result = query.execute(&index.snapshot(), &mut Shared(&work))?;
                // Native empty and zero-k paths still have a final live gate.
                work.borrow_mut().charge(1)?;
                Ok(result)
            })();
            match work.into_inner().interrupted {
                Some(error) => Err(ReadError::Interrupted(error)),
                None => result.map_err(ReadError::Index),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch, WriteError};
    use asupersync::lab::run_async_under_lab;
    use fgdb_beacon::{DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch};
    use fgdb_delta_types::RelationId;
    use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts};

    const LABEL: LabelId = LabelId(1);
    const TEXT: PropertyKeyId = PropertyKeyId(1);
    const X: PropertyKeyId = PropertyKeyId(2);
    const Y: PropertyKeyId = PropertyKeyId(3);

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new([0xc1; 32], DatabaseSecurityNamespaceId([0xc2; 32]), [0xc3; 32])
    }

    fn options() -> Options {
        let mut options = Options::text(TEXT);
        options.vertex_label = Some(LABEL);
        options.projection.vector = vec![X, Y];
        options.index.vector = Some(HnswConfig::new(2, DistanceMetric::SquaredEuclidean));
        options
    }

    fn props(text: &str, x: i64, y: i64) -> Vec<(PropertyKeyId, CanonicalScalar)> {
        vec![
            (TEXT, CanonicalScalar::Text(text.into())),
            (X, CanonicalScalar::Int(x)),
            (Y, CanonicalScalar::Int(y)),
        ]
    }

    fn text(limit: usize) -> Search<'static> {
        Search::Text { query: "rust", k: limit, mode: TextMatch::Any }
    }

    #[test]
    fn staged_text_vectors_and_hybrid_match_the_committed_corpus() {
        let ((), report) = run_async_under_lab(0xbeac_1001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query_cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            seed.create_vertex(VId(1), vec![LABEL], props("old", 0, 1));
            seed.create_vertex(VId(2), vec![LABEL], props("rust removed", 0, 1));
            seed.create_vertex(VId(3), vec![], props("rust graph", 1, 1));
            seed.create_vertex(VId(4), vec![LABEL], props("rust filtered", 1, 0));
            db.write(&commit, seed).await.unwrap();
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            txn.savepoint(&db, "before-search").unwrap();
            let mut changes = WriteBatch::new(RelationId(1));
            changes.set_vertex_property(VId(1), TEXT, Some(CanonicalScalar::Text("rust systems".into())));
            changes.set_vertex_property(VId(1), X, Some(CanonicalScalar::Int(1)));
            changes.set_vertex_property(VId(1), Y, Some(CanonicalScalar::Int(0)));
            changes.delete_vertex(VId(2));
            changes.set_vertex_label(VId(3), LABEL, true);
            changes.set_vertex_label(VId(4), LABEL, false);
            changes.create_vertex(VId(5), vec![LABEL], props("rust database", 2, 1));
            changes.create_vertex(VId(6), vec![LABEL], props("erased", 1, 0));
            changes.delete_vertex(VId(6));
            txn.write(&mut db, changes).unwrap();
            let opts = options();
            let queries = [
                text(10),
                Search::Vector { query: &[1.0, 0.0], k: 10, mode: VectorSearch::Exact },
                Search::Vector { query: &[1.0, 0.0], k: 10, mode: VectorSearch::Approximate { ef_search: 32 } },
                Search::Hybrid(ExactHybridQuery {
                    vector: &[1.0, 0.0], text: "rust", k: 10,
                    vector_candidates: 10, text_candidates: 10,
                    vector_mode: VectorSearch::Exact, text_mode: TextMatch::Any,
                    profile: ExactRrfProfile::default(),
                }),
            ];
            let staged: Vec<_> = queries.iter().map(|search| {
                txn.beacon_search(&db, &query_cx, &opts, *search).unwrap()
            }).collect();
            assert!(matches!(&staged[0], Rows::Text(hits) if hits.len() == 3));
            assert_eq!(db.frontier().unwrap(), basis, "search must not publish");
            assert!(!txn.scanned_vertices.get());
            assert!(txn.read_set.borrow().contains(&ElementId::Vertex(VId(6))));
            txn.commit(&mut db, &commit).await.unwrap();
            for (search, expected) in queries.into_iter().zip(staged) {
                assert_eq!(db.beacon_search(&query_cx, &opts, search).unwrap(), expected);
            }
            assert_eq!(contexts.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn searches_keep_the_pinned_basis_and_retain_non_hit_read_conflicts() {
        let ((), report) = run_async_under_lab(0xbeac_1002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query_cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            seed.create_vertex(VId(1), vec![LABEL], props("rust", 1, 0));
            seed.create_vertex(VId(2), vec![LABEL], props("no match", 0, 1));
            db.write(&commit, seed).await.unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let opts = options();
            let before = txn.beacon_search(&db, &query_cx, &opts, text(1)).unwrap();
            assert!(matches!(&before, Rows::Text(hits) if hits.len() == 1));
            assert!(txn.read_set.borrow().contains(&ElementId::Vertex(VId(2))));
            let mut winner = WriteBatch::new(RelationId(1));
            winner.set_vertex_property(VId(2), TEXT, Some(CanonicalScalar::Text("rust rust".into())));
            db.write(&commit, winner).await.unwrap();
            assert_eq!(txn.beacon_search(&db, &query_cx, &opts, text(1)).unwrap(), before);
            assert!(matches!(txn.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
            assert_eq!(contexts.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn empty_searches_keep_label_phantoms_and_normalized_negative_observations() {
        for case in 0..3 {
            let ((), report) = run_async_under_lab(0xbeac_1003 + case, move |root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let query_cx = contexts.query();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut txn = db.begin(&contexts.txn()).unwrap();
                txn.savepoint(&db, "clean").unwrap();
                if case == 2 {
                    let mut erased = WriteBatch::new(RelationId(1));
                    erased.create_vertex(VId(9), vec![], vec![]);
                    erased.delete_vertex(VId(9));
                    txn.write(&mut db, erased).unwrap();
                }
                assert!(matches!(txn.beacon_search(&db, &query_cx, &options(), text(0)).unwrap(),
                    Rows::Text(hits) if hits.is_empty()));
                txn.rollback_to_savepoint(&db, "clean").unwrap();
                let mut winner = WriteBatch::new(RelationId(1));
                winner.create_vertex(VId(9), if case == 1 { vec![LABEL] } else { vec![] },
                    props("rust", 1, 0));
                db.write(&commit, winner).await.unwrap();
                let result = txn.finish(&mut db, &commit).await;
                if case == 0 {
                    assert!(result.is_ok(), "unrelated-label insertion is not a phantom: {result:?}");
                } else {
                    assert!(matches!(result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
                }
                assert_eq!(contexts.outstanding_obligations(), 0);
            });
            assert!(report.lab_test_passed(), "{report:?}");
        }
    }

    #[test]
    fn limits_and_wrong_contexts_refuse_without_finishing_or_publishing() {
        let ((), report) = run_async_under_lab(0xbeac_1006, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query_cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let other = Database::open_memory(&commit, keys()).await.unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let mut staged = WriteBatch::new(RelationId(1));
            staged.create_vertex(VId(1), vec![LABEL], props("rust", 1, 0));
            txn.write(&mut db, staged).unwrap();
            for case in 0..3 {
                let mut opts = options();
                match case {
                    0 => opts.policy.max_work_units = 0,
                    1 => opts.policy.max_source_scratch = 0,
                    _ => opts.policy.max_staging_rows = 0,
                }
                let result = txn.beacon_search(&db, &query_cx, &opts, text(1));
                if case == 0 {
                    assert!(matches!(result, Err(ReadError::Index(BeaconError::WorkBudgetExceeded))));
                } else {
                    assert!(matches!(result, Err(ReadError::Index(BeaconError::ResourceLimit { .. }))));
                }
            }
            let mut historical = options();
            historical.as_of = Some(CommitSeq(txn.basis().0 + 1));
            assert!(matches!(txn.beacon_search(&db, &query_cx, &historical, text(1)),
                Err(ReadError::Index(BeaconError::InvalidQuery(_)))));
            assert!(matches!(txn.beacon_search(&other, &query_cx, &options(), text(1)),
                Err(ReadError::Read(WriteTxnError::WrongDatabase))));
            assert_eq!(db.frontier().unwrap(), txn.basis());
            assert!(matches!(txn.beacon_search(&db, &query_cx, &options(), text(1)).unwrap(),
                Rows::Text(hits) if hits.len() == 1));
            txn.abort();
            assert!(matches!(txn.beacon_search(&db, &query_cx, &options(), text(1)),
                Err(ReadError::Read(WriteTxnError::Finished))));
            assert_eq!(contexts.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
