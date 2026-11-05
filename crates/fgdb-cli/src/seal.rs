//! Explicit durable relation sealing. Admission flags bind before opening the
//! database; the sole result follows the authenticated root-slot publication.

use super::{Failure, decimal_usize, emit, execution_failure, hex, set_decimal};
use asupersync::fs::Vfs;
use fgdb::{Database, OriginBirthError, SealPublicationError};
use fgdb_delta_types::RelationId;
use fgdb_strata::store::RootReadLimits;
use fgdb_strata::tiered::sealed::{AdjRunError, SealedError, SealedLimits};
use fgdb_types::{CommitSeq, PurposeContexts};
use std::collections::BTreeSet;
use std::io::Write;

#[derive(Default)]
pub(super) struct SealOptions {
    relation: Option<u64>,
    floor: Option<u64>,
    sealing: SealedLimits,
    source: RootReadLimits,
    limits_set: BTreeSet<String>,
}

impl SealOptions {
    pub(super) const FLAGS: [&str; 12] = [
        "--seal-relation",
        "--retain-from",
        "--sealed-max-rows",
        "--sealed-max-incidences",
        "--sealed-image-bytes",
        "--sealed-property-bytes",
        "--seal-source-root-bytes",
        "--seal-source-bytes",
        "--seal-source-blocks",
        "--seal-source-vertex-patches",
        "--seal-source-incidences",
        "--seal-source-vertex-versions",
    ];

    pub(super) fn set(&mut self, flag: &str, raw: &str) -> Result<(), Failure> {
        match flag {
            "--seal-relation" => return set_decimal(&mut self.relation, flag, raw),
            "--retain-from" => return set_decimal(&mut self.floor, flag, raw),
            _ => {}
        }
        let target = match flag {
            "--sealed-max-rows" => &mut self.sealing.max_rows,
            "--sealed-max-incidences" => &mut self.sealing.max_incidences,
            "--sealed-image-bytes" => &mut self.sealing.max_image_bytes,
            "--sealed-property-bytes" => &mut self.sealing.max_property_bytes,
            "--seal-source-root-bytes" => &mut self.source.max_root_bytes,
            "--seal-source-bytes" => &mut self.source.max_source_bytes,
            "--seal-source-blocks" => &mut self.source.max_blocks,
            "--seal-source-vertex-patches" => &mut self.source.max_vertex_patches,
            "--seal-source-incidences" => &mut self.source.max_incidences,
            "--seal-source-vertex-versions" => &mut self.source.max_vertex_versions,
            _ => return Err(Failure::usage("unknown seal flag")),
        };
        if !self.limits_set.insert(flag.to_owned()) {
            return Err(Failure::usage(format!("{flag} must be supplied only once")));
        }
        *target = decimal_usize(flag, raw)?;
        Ok(())
    }

    pub(super) fn validate(&self) -> Result<(), Failure> {
        if self.relation.is_none() {
            return Err(Failure::usage(
                "seal requires --seal-relation <decimal-u64>",
            ));
        }
        Ok(())
    }
}

pub(super) async fn run<V: Vfs + Clone>(
    db: &mut Database<V>,
    contexts: &PurposeContexts,
    options: &SealOptions,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let relation = options
        .relation
        .expect("seal relation validated before open");
    let floor = options.floor.unwrap_or(0);
    let run = db
        .seal_relation(
            &contexts.commit(),
            &contexts.query(),
            RelationId(relation),
            CommitSeq(floor),
            options.sealing,
            options.source,
        )
        .await
        .map_err(seal_failure)?;
    let seq = db.frontier().map_err(Failure::io)?.0;
    let run = hex(&run.0.0);
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"sealed","seq":{seq},"relation":{relation},"retain_from":{floor},"run":"{run}"}}"#
            ),
        )
    } else {
        writeln!(
            out,
            "sealed relation {relation} for sequences {floor}..={seq} (run {run})"
        )
        .map_err(Failure::io)
    }
}

fn seal_failure(error: SealPublicationError) -> Failure {
    match error {
        SealPublicationError::Publication(fgdb::RebuildError::Store(error)) => {
            super::fnx::sealed_store_failure(error)
        }
        SealPublicationError::Source(error)
        | SealPublicationError::Run(AdjRunError::Sealed(error)) => match error {
            SealedError::Store(error) => super::fnx::sealed_store_failure(*error),
            error @ SealedError::Limit { .. } => {
                Failure::coded(super::refusal::Code::Budget, error)
            }
            SealedError::InvalidFloor => {
                Failure::invalid_value("--retain-from exceeds the published graph frontier")
            }
            error => execution_failure(error),
        },
        SealPublicationError::Origin(error @ OriginBirthError::Limit { .. }) => {
            Failure::coded(super::refusal::Code::Budget, error)
        }
        error => execution_failure(error),
    }
}

#[cfg(test)]
mod tests;
