//! Operator-declared, commit-maintained retrieval indexes for served databases.
//!
//! These definitions are process configuration, not a durable catalog. Startup
//! and authoritative recovery restore authenticated Chronicle-watermarked
//! checkpoints, replay their retained delta tails, or rebuild before publishing
//! a readable handle. Checkpointing is separate from ordinary graph commits.
//! Queries still select only exact-cut, compatible, fully visible generations
//! through the engine's existing Beacon admission path.

use asupersync::fs::Vfs;
use fgdb::{
    Database, DerivedCheckpointError, DerivedCheckpointOutcome, MaintainedBeaconError,
    MaintainedBeaconIndex, MaintainedBeaconStatus, ResidentIndexError,
};
use fgdb_beacon::read::{ReadOptions, ReadPolicy};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{CommitCx, CommitSeq, QueryCx};
use std::collections::BTreeSet;

/// One named, bounded text/vector index owned by a served database.
///
/// The name identifies operator diagnostics and the derived checkpoint;
/// `CALL hybrid.search` continues to select indexes by their complete definition,
/// visibility and snapshot. No query can change these definitions or borrow
/// their maintenance allowance.
#[derive(Clone, Debug)]
pub struct BeaconIndexConfig {
    pub name: String,
    pub options: ReadOptions<PropertyKeyId, LabelId>,
    /// Separate per-commit budget. Exhaustion fences the derived index without
    /// changing the outcome of a durable graph write; ordinary query fallback
    /// retains the request's own authorization and resource ceilings.
    pub maintenance: ReadPolicy,
}

impl BeaconIndexConfig {
    /// Use the initial build's policy for each subsequent committed update.
    /// Hosts can replace `maintenance` independently before opening the server.
    #[must_use]
    pub fn new(name: impl Into<String>, options: ReadOptions<PropertyKeyId, LabelId>) -> Self {
        Self {
            name: name.into(),
            maintenance: options.policy,
            options,
        }
    }
}

/// Trusted-host diagnostic for one configured index in the current generation.
/// This is never exposed by an unauthenticated or capability-holder endpoint.
#[derive(Clone, Debug)]
pub struct BeaconIndexStatus {
    pub name: String,
    pub status: MaintainedBeaconStatus,
    /// How this served generation obtained its initial index. Later commits
    /// advance `status`; they do not rewrite this admission receipt.
    pub restoration: DerivedCheckpointOutcome,
    /// The last checkpoint known to have been durably saved or authenticated
    /// on this generation. It may lag a healthy commit-maintained index.
    pub checkpoint_sequence: Option<CommitSeq>,
    /// The last checkpoint save failure, if any. It does not fence the index
    /// or database; a later successful checkpoint clears the diagnostic.
    pub checkpoint_error: Option<String>,
}

/// One trusted-host checkpoint attempt. Each configured index reports its own
/// outcome, so a failed sibling never masquerades as a complete save.
#[derive(Debug)]
pub struct BeaconCheckpointReport {
    pub name: String,
    pub result: Result<CommitSeq, DerivedCheckpointError>,
}

#[derive(Clone)]
pub(crate) struct ConfiguredIndex {
    pub(crate) name: String,
    pub(crate) handle: MaintainedBeaconIndex,
    pub(crate) restoration: DerivedCheckpointOutcome,
    pub(crate) checkpoint_sequence: Option<CommitSeq>,
    pub(crate) checkpoint_error: Option<String>,
}

pub(crate) struct ConfiguredIndexes {
    pub(crate) definitions: Vec<BeaconIndexConfig>,
    pub(crate) handles: Vec<ConfiguredIndex>,
}

/// Validate the entire bounded declaration before opening a database or
/// building a first sibling. Source-dependent errors remain registration errors.
pub(crate) fn validate(definitions: &[BeaconIndexConfig]) -> Result<(), crate::ServerError> {
    let invalid = |name: &str, reason: &str| crate::ServerError::BeaconConfig {
        name: name.to_owned(),
        reason: reason.to_owned(),
    };
    if definitions.len() > fgdb::MAX_MAINTAINED_BEACON_INDEXES {
        return Err(invalid(
            "",
            "at most 64 configured Beacon indexes are allowed",
        ));
    }
    let mut names = BTreeSet::new();
    for definition in definitions {
        let name = &definition.name;
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        {
            return Err(invalid(
                name,
                "index names must be 1..=128 bytes of [A-Za-z0-9_.-]",
            ));
        }
        if !names.insert(name) {
            return Err(invalid(name, "duplicate configured index name"));
        }
        let options = &definition.options;
        if options.as_of.is_some() {
            return Err(invalid(
                name,
                "a maintained server index must follow the current frontier",
            ));
        }
        options
            .index
            .validate()
            .map_err(|error| invalid(name, &error.to_string()))?;
        if options.index.text.is_some() != options.projection.text.is_some() {
            return Err(invalid(
                name,
                "a text lane requires exactly one text property",
            ));
        }
        match &options.index.vector {
            Some(vector) if !options.projection.fits(vector.dimensions) => {
                return Err(invalid(
                    name,
                    "vector properties must match the declared encoding and dimensions",
                ));
            }
            None if !options.projection.vector.is_empty() => {
                return Err(invalid(name, "vector properties require a vector lane"));
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) async fn register<V: Vfs + Clone>(
    commit_cx: &CommitCx,
    query_cx: &QueryCx,
    database: &mut Database<V>,
    definitions: &[BeaconIndexConfig],
) -> Result<Vec<ConfiguredIndex>, crate::ServerError> {
    let mut indexes = Vec::with_capacity(definitions.len());
    for definition in definitions {
        let failure = |source| crate::ServerError::BeaconIndex {
            name: definition.name.clone(),
            source,
        };
        let (handle, restoration) = database
            .load_or_rebuild_beacon_checkpoint(
                commit_cx,
                query_cx,
                &definition.name,
                &definition.options,
                definition.maintenance,
            )
            .await
            .map_err(failure)?;
        // Checkpoint publication is an optional acceleration, never a new
        // authority or a reason to discard an already healthy live index.
        // Cancellation remains terminal even when it arrives during saving.
        let (checkpoint_sequence, checkpoint_error) = match restoration {
            DerivedCheckpointOutcome::CatchUp {
                watermark_seq,
                head_seq,
                healed_by_raptorq: false,
                ..
            } if watermark_seq == head_seq => (Some(head_seq), None),
            _ => match database
                .save_beacon_checkpoint(commit_cx, query_cx, &handle, &definition.name)
                .await
            {
                Ok(sequence) => (Some(sequence), None),
                Err(source) if checkpoint_interrupted(&source) => return Err(failure(source)),
                Err(source) => (None, Some(source.to_string())),
            },
        };
        commit_cx
            .checkpoint()
            .map_err(|error| failure(DerivedCheckpointError::Interrupted(error)))?;
        query_cx
            .checkpoint()
            .map_err(|error| failure(DerivedCheckpointError::Interrupted(error)))?;
        indexes.push(ConfiguredIndex {
            name: definition.name.clone(),
            handle,
            restoration,
            checkpoint_sequence,
            checkpoint_error,
        });
    }
    Ok(indexes)
}

fn checkpoint_interrupted(error: &DerivedCheckpointError) -> bool {
    matches!(
        error,
        DerivedCheckpointError::Interrupted(_)
            | DerivedCheckpointError::Beacon(MaintainedBeaconError::Resident(
                ResidentIndexError::Interrupted(_)
                    | ResidentIndexError::Index(fgdb_beacon::BeaconError::Cancelled)
            ))
    )
}

/// The caller serializes this operation with other checkpoints and writers.
/// Saving derived data never changes a committed graph outcome or retries a
/// failed graph operation. Automatic maintenance can retain an already-current
/// healthy checkpoint; an explicit host request forces a fresh save. Every
/// sibling keeps an explicit result, including a retained checkpoint's cut.
pub(crate) async fn checkpoint<V: Vfs + Clone>(
    commit: &CommitCx,
    query: &QueryCx,
    database: &mut Database<V>,
    indexes: &mut [ConfiguredIndex],
    force: bool,
) -> Vec<BeaconCheckpointReport> {
    let mut reports = Vec::with_capacity(indexes.len());
    for index in indexes {
        let result = async {
            commit
                .checkpoint()
                .map_err(DerivedCheckpointError::Interrupted)?;
            if !force && index.checkpoint_error.is_none() {
                let status = database
                    .beacon_index_status(query, &index.handle)
                    .map_err(DerivedCheckpointError::Beacon)?;
                if status.failure.is_none()
                    && status.source_sequence == status.database_sequence
                    && index.checkpoint_sequence == Some(status.source_sequence)
                {
                    return Ok(status.source_sequence);
                }
            }
            database
                .save_beacon_checkpoint(commit, query, &index.handle, &index.name)
                .await
        }
        .await;
        match &result {
            Ok(sequence) => {
                index.checkpoint_sequence = Some(*sequence);
                index.checkpoint_error = None;
            }
            Err(error) => index.checkpoint_error = Some(error.to_string()),
        }
        reports.push(BeaconCheckpointReport {
            name: index.name.clone(),
            result,
        });
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_beacon::{DistanceMetric, HnswConfig};
    use fgdb_types::CommitSeq;

    fn text() -> BeaconIndexConfig {
        BeaconIndexConfig::new("documents", ReadOptions::text(PropertyKeyId(1)))
    }

    #[test]
    fn configuration_rejects_ambiguous_historical_and_unbounded_declarations() {
        assert!(validate(&[text()]).is_ok());
        assert!(validate(&[text(), text()]).is_err());
        assert!(validate(&vec![text(); fgdb::MAX_MAINTAINED_BEACON_INDEXES + 1]).is_err());
        for name in ["", "with space", "nested/path"] {
            let mut index = text();
            index.name = name.into();
            assert!(validate(&[index]).is_err());
        }
        let mut index = text();
        index.options.as_of = Some(CommitSeq(0));
        assert!(validate(&[index]).is_err());
        let mut index = text();
        index.options.index.max_documents = 0;
        assert!(validate(&[index]).is_err());
    }

    #[test]
    fn configuration_checks_both_lane_projections_before_source_access() {
        let mut index = text();
        index.options.projection.text = None;
        assert!(validate(&[index]).is_err());
        let mut index = text();
        index.options.projection.vector = vec![PropertyKeyId(2)];
        assert!(validate(&[index.clone()]).is_err());
        index.options.index.vector = Some(HnswConfig::new(2, DistanceMetric::SquaredEuclidean));
        assert!(validate(&[index.clone()]).is_err());
        index.options.projection.vector.push(PropertyKeyId(3));
        assert!(validate(&[index]).is_ok());
    }

    #[test]
    fn checkpoint_save_interruption_never_becomes_an_advisory_failure() {
        use asupersync::lab::run_async_under_lab;
        use fgdb::{DatabaseKeys, MemVfs};
        use fgdb_types::context::SimulationCheckpointProbe;
        use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
        use std::sync::Arc;

        let ((), report) = run_async_under_lab(0xbeac_d001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let keys = DatabaseKeys::new(
                [0x51; 32],
                DatabaseSecurityNamespaceId([0x52; 32]),
                [0x53; 32],
            );
            let definition = text();
            let vfs = MemVfs::new().unwrap();
            let mut baseline =
                Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys.clone())
                    .await
                    .unwrap();
            let loading = Arc::new(SimulationCheckpointProbe::new(None));
            let (handle, _) = baseline
                .load_or_rebuild_beacon_checkpoint(
                    &commit,
                    &query.with_checkpoint_probe(Arc::clone(&loading)),
                    &definition.name,
                    &definition.options,
                    definition.maintenance,
                )
                .await
                .unwrap();
            let saving = Arc::new(SimulationCheckpointProbe::new(None));
            baseline
                .save_beacon_checkpoint(
                    &commit,
                    &query.with_checkpoint_probe(Arc::clone(&saving)),
                    &handle,
                    &definition.name,
                )
                .await
                .unwrap();
            assert!(saving.calls() > 1);

            // The probe interrupts once and then permits later checkpoints.
            // Therefore a final recheck alone cannot hide a swallowed save
            // interruption. Include the final post-save publication check.
            for save_checkpoint in 1..=saving.calls() + 1 {
                let vfs = MemVfs::new().unwrap();
                let mut database = Database::create_with_vfs(
                    &commit,
                    vfs.clone(),
                    vfs.database_dir(),
                    keys.clone(),
                )
                .await
                .unwrap();
                let stop = loading.calls() + save_checkpoint;
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let interrupted = query.with_checkpoint_probe(Arc::clone(&probe));
                let result = register(
                    &commit,
                    &interrupted,
                    &mut database,
                    std::slice::from_ref(&definition),
                )
                .await;
                assert!(
                    matches!(
                        result,
                        Err(crate::ServerError::BeaconIndex { ref source, .. })
                            if checkpoint_interrupted(source)
                    ),
                    "save checkpoint {save_checkpoint} was not terminal"
                );
                assert_eq!(probe.calls(), stop);
                assert_eq!(database.frontier().unwrap(), CommitSeq(0));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn automatic_checkpoint_preserves_one_shot_status_interruption() {
        use asupersync::lab::run_async_under_lab;
        use fgdb::{DatabaseKeys, MemVfs};
        use fgdb_types::context::SimulationCheckpointProbe;
        use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
        use std::sync::Arc;

        let ((), report) = run_async_under_lab(0xbeac_d002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let keys = DatabaseKeys::new(
                [0x61; 32],
                DatabaseSecurityNamespaceId([0x62; 32]),
                [0x63; 32],
            );
            let vfs = MemVfs::new().unwrap();
            let mut database =
                Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys)
                    .await
                    .unwrap();
            let mut indexes = register(&commit, &query, &mut database, &[text()])
                .await
                .unwrap();
            assert_eq!(indexes[0].checkpoint_sequence, Some(CommitSeq(0)));

            // Only the status probe interrupts; a later save would succeed if
            // that error were discarded while deciding whether to skip it.
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(1)));
            let interrupted = query.with_checkpoint_probe(Arc::clone(&probe));
            let reports =
                checkpoint(&commit, &interrupted, &mut database, &mut indexes, false).await;
            assert_eq!(reports.len(), 1);
            assert!(
                reports[0]
                    .result
                    .as_ref()
                    .is_err_and(checkpoint_interrupted)
            );
            assert_eq!(probe.calls(), 1);
            assert!(indexes[0].checkpoint_error.is_some());
            assert_eq!(indexes[0].checkpoint_sequence, Some(CommitSeq(0)));
            assert_eq!(database.frontier().unwrap(), CommitSeq(0));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
