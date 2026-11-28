//! Offline repair from explicitly supplied native Chronicle containers.

use std::path::{Path, PathBuf};

use asupersync::io::AsyncReadExt as _;
use fgdb::{Database, DatabaseKeys};
use fgdb_chronicle::capsule::MAX_CAPSULE_CONTAINER_BYTES_V1;
use fgdb_chronicle::transfer::VerifiedObject;
use fgdb_types::{CommitCx, ObjectId};

use super::Failure;

const MAX_FILES: usize = 64;
const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Default)]
pub(crate) struct Inputs {
    files: Vec<(ObjectId, PathBuf)>,
}

impl Inputs {
    pub(crate) fn add(&mut self, oid: &str, path: &str) -> Result<(), Failure> {
        if self.files.len() == MAX_FILES {
            return Err(Failure::usage("at most 64 backup capsules may be supplied"));
        }
        if oid.len() != 64 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Failure::usage(
                "--restore-capsule requires a 64-digit hexadecimal object ID",
            ));
        }
        let mut bytes = [0; 32];
        for (slot, pair) in bytes.iter_mut().zip(oid.as_bytes().chunks_exact(2)) {
            let digits = std::str::from_utf8(pair).map_err(Failure::usage)?;
            *slot = u8::from_str_radix(digits, 16).map_err(Failure::usage)?;
        }
        let id = ObjectId(bytes);
        if self.files.iter().any(|(other, _)| other == &id) {
            return Err(Failure::usage("duplicate backup capsule object ID"));
        }
        self.files.push((id, PathBuf::from(path)));
        Ok(())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub(super) fn object_ids(&self) -> impl Iterator<Item = &ObjectId> {
        self.files.iter().map(|(id, _)| id)
    }

    pub(crate) async fn open(
        &self,
        cx: &CommitCx,
        database: &Path,
        keys: DatabaseKeys,
    ) -> Result<Database, Failure> {
        let capsule_keys = keys.capsule_keys();
        let mut recovered = Vec::with_capacity(self.files.len());
        let mut remaining = MAX_INPUT_BYTES;
        for (id, path) in &self.files {
            cx.checkpoint().map_err(Failure::io)?;
            let ceiling = remaining.min(MAX_CAPSULE_CONTAINER_BYTES_V1);
            let named = asupersync::fs::metadata(path).await.map_err(Failure::io)?;
            if !named.is_file() {
                return Err(Failure::usage("backup capsule must be a regular file"));
            }
            let file = asupersync::fs::File::open(path)
                .await
                .map_err(Failure::io)?;
            let opened = file.metadata().await.map_err(Failure::io)?;
            if !opened.is_file() || opened.len() > ceiling as u64 {
                return Err(Failure::usage(
                    "backup capsule exceeds the file or aggregate input limit",
                ));
            }
            let mut bytes = Vec::new();
            file.take(ceiling as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(Failure::io)?;
            if bytes.len() > ceiling {
                return Err(Failure::usage("backup capsule exceeds the input limit"));
            }
            remaining -= bytes.len();
            let mut verification = Vec::new();
            recovered.push(
                VerifiedObject::from_capsule_container(
                    &capsule_keys,
                    *id,
                    &bytes,
                    &mut verification,
                )
                .map_err(Failure::open)?,
            );
        }
        cx.checkpoint().map_err(Failure::io)?;
        Database::open_restoring_capsules(cx, database, keys, &recovered)
            .await
            .map_err(|error| match error {
                fgdb::CapsuleRecoveryError::Open(error) => crate::open_failure(error),
                fgdb::CapsuleRecoveryError::Capsule { .. } => Failure::io(error),
            })
    }
}
