//! Operator-declared, commit-maintained retrieval indexes for served databases.
//!
//! These definitions are process configuration, not a durable catalog. Startup
//! and authoritative recovery build them before publishing a readable handle.
//! Queries still select only exact-cut, compatible, fully visible generations
//! through the engine's existing Beacon admission path.

use fgdb::{Database, MaintainedBeaconIndex, MaintainedBeaconStatus};
use fgdb_beacon::read::{ReadOptions, ReadPolicy};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::QueryCx;
use std::collections::BTreeSet;

/// One named, bounded text/vector index owned by a served database.
///
/// The name identifies operator diagnostics; `CALL hybrid.search` continues to
/// select indexes by their complete definition, visibility and snapshot. No
/// query can change these definitions or borrow their maintenance allowance.
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
}

pub(crate) struct ConfiguredIndexes {
    pub(crate) definitions: Vec<BeaconIndexConfig>,
    pub(crate) handles: Vec<(String, MaintainedBeaconIndex)>,
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

pub(crate) fn register(
    cx: &QueryCx,
    database: &mut Database,
    definitions: &[BeaconIndexConfig],
) -> Result<Vec<(String, MaintainedBeaconIndex)>, crate::ServerError> {
    definitions
        .iter()
        .map(|definition| {
            database
                .register_beacon_index(cx, &definition.options, definition.maintenance)
                .map(|handle| (definition.name.clone(), handle))
                .map_err(|source| crate::ServerError::BeaconIndex {
                    name: definition.name.clone(),
                    source,
                })
        })
        .collect()
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
}
