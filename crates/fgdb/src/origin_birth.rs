//! Immutable edge origins reconstructed from the creating Chronicle effects.
//!
//! Tier-D `created_at` values describe content versions. Property updates move
//! those starts, so neither a current row nor an arbitrary retained row can
//! supply immutable creation order. Explicit Tier-R materialization instead
//! authenticates the complete prefix through its source publication and keeps
//! only the origins of the sealed image's selected stable identities.

use std::collections::{BTreeMap, BTreeSet};

use asupersync::fs::Vfs;
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{DeltaRow, LogicalDeltaBatch};
use fgdb_strata::tiered::sealed::OriginBirthOrder;
use fgdb_types::{BranchId, CommitCx, CommitSeq, EId, GraphId};

use crate::{CryptoVerificationEvent, DatabaseKeys, RebuildError, read_delta_batch};

/// An origin proof is incomplete, contradictory, or exceeds its explicit cut
/// or retained-identity allowance. Failure never returns a partial map.
#[derive(Debug)]
pub enum OriginBirthError {
    /// Authenticating the original committed effects failed.
    Source(RebuildError),
    /// The requested source publication is absent from the recovered chain.
    CutBeyondFrontier {
        through: CommitSeq,
        frontier: CommitSeq,
    },
    /// No first creation for a retained identity exists in the selected prefix.
    Missing { eid: EId, through: CommitSeq },
    /// An immutable identity has two creation effects in the selected prefix.
    Duplicate {
        eid: EId,
        first: OriginBirthOrder<EId>,
        duplicate: OriginBirthOrder<EId>,
    },
    /// The selected identities exceed the caller's explicit retention bound.
    Limit { requested: usize, limit: usize },
}

impl core::fmt::Display for OriginBirthError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => write!(f, "edge origin source: {error}"),
            Self::CutBeyondFrontier { through, frontier } => write!(
                f,
                "edge origin publication {through:?} exceeds recovered frontier {frontier:?}"
            ),
            Self::Missing { eid, through } => write!(
                f,
                "edge {eid:?} has no creation in the authenticated prefix through {through:?}"
            ),
            Self::Duplicate { eid, .. } => {
                write!(f, "edge {eid:?} has more than one committed creation")
            }
            Self::Limit { requested, limit } => write!(
                f,
                "edge origin identities require {requested} entries, above the limit {limit}"
            ),
        }
    }
}

impl core::error::Error for OriginBirthError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RebuildError> for OriginBirthError {
    fn from(error: RebuildError) -> Self {
        Self::Source(error)
    }
}

/// Coordinates and exact retained identities of the admitted source image.
/// The caller derives these from its sealed partition, never from run bytes.
pub(crate) struct EdgeOriginRequest<'a> {
    pub graph: GraphId,
    pub branch: BranchId,
    pub through: CommitSeq,
    pub edge_ids: &'a BTreeSet<EId>,
    pub max_origins: usize,
}

/// Authenticate every capsule in the selected prefix, including capsules
/// after the last requested creation and capsules for unrelated coordinates.
/// Only the selected EIds occupy the resulting map; one format-bounded
/// decoded batch is transient at a time. This does not materialize or change
/// the database's delta window, writer, version heads, or publication.
pub(crate) async fn reconstruct<V: Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
    keys: &DatabaseKeys,
    request: EdgeOriginRequest<'_>,
    verification: &mut Vec<CryptoVerificationEvent>,
) -> Result<BTreeMap<EId, OriginBirthOrder<EId>>, OriginBirthError> {
    checkpoint(cx)?;
    if request.edge_ids.len() > request.max_origins {
        return Err(OriginBirthError::Limit {
            requested: request.edge_ids.len(),
            limit: request.max_origins,
        });
    }
    let entries = coordinator.chain().entries();
    let frontier = entries.last().map_or(CommitSeq::ORIGIN, |entry| {
        CommitSeq(entry.marker.commit_seq)
    });
    if request.through > frontier {
        return Err(OriginBirthError::CutBeyondFrontier {
            through: request.through,
            frontier,
        });
    }

    let mut origins = BTreeMap::new();
    let end = entries.partition_point(|entry| entry.marker.commit_seq <= request.through.0);
    for entry in &entries[..end] {
        // Keep this profile match exhaustive: adding merge/import effects to
        // Chronicle must also provide their immutable-origin rule here.
        let fgdb_chronicle::marker::EffectSource::Local { .. } = &entry.marker.effect_source;
        let batch = read_delta_batch(cx, coordinator, keys, entry, verification).await?;
        retain_origins(cx, &batch, &request, &mut origins)?;
    }
    for eid in request.edge_ids {
        checkpoint(cx)?;
        if !origins.contains_key(eid) {
            return Err(OriginBirthError::Missing {
                eid: *eid,
                through: request.through,
            });
        }
    }
    checkpoint(cx)?;
    Ok(origins)
}

fn retain_origins(
    cx: &CommitCx,
    batch: &LogicalDeltaBatch,
    request: &EdgeOriginRequest<'_>,
    origins: &mut BTreeMap<EId, OriginBirthOrder<EId>>,
) -> Result<(), OriginBirthError> {
    for coordinate in batch.coordinate_entries() {
        checkpoint(cx)?;
        if (coordinate.graph, coordinate.branch) != (request.graph, request.branch) {
            continue;
        }
        for row in &coordinate.rows {
            checkpoint(cx)?;
            let DeltaRow::CreateEdge {
                eid, birth_ordinal, ..
            } = row
            else {
                continue;
            };
            if !request.edge_ids.contains(eid) {
                continue;
            }
            // The current Chronicle effect profile has only Local creations;
            // there is no merge/import arm and therefore no merge expansion
            // ordinal. Preserve the exact sequence-neutral intent ordinal and
            // attach its ACTUAL committed marker sequence, never a later
            // content-version start. A future import profile must preserve its
            // source origin instead of flowing through this local-only rule.
            let origin = OriginBirthOrder::new(batch.commit_seq(), *birth_ordinal, 0, *eid);
            if let Some(first) = origins.get(eid) {
                return Err(OriginBirthError::Duplicate {
                    eid: *eid,
                    first: *first,
                    duplicate: origin,
                });
            }
            // The request's size was checked before reading any capsules, and
            // only its members enter this map. Keep the bound immediately
            // before growth as well, without copying unrelated graph origins.
            if origins.len() >= request.max_origins {
                return Err(OriginBirthError::Limit {
                    requested: request.edge_ids.len(),
                    limit: request.max_origins,
                });
            }
            origins.insert(*eid, origin);
        }
    }
    Ok(())
}

fn checkpoint(cx: &CommitCx) -> Result<(), OriginBirthError> {
    cx.checkpoint()
        .map_err(RebuildError::Interrupted)
        .map_err(OriginBirthError::Source)
}

#[cfg(test)]
mod tests;
