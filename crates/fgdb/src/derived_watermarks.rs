//! Durable Chronicle-watermarked checkpoints for Beacon indexes and Ripple
//! standing-query Z-set arrangements (`fgdb-rc1111-durable-watermarks-c1g98`,
//! `FG-INV-18`).
//!
//! Every derived checkpoint stored under `<db_dir>/derived/` is:
//! 1. **Never authoritative (`FG-INV-18`)**: bound to `(watermark_seq,
//!    chain_commitment_at(watermark_seq))`. Any checkpoint whose watermark
//!    exceeds `committed_head_seq`, disagrees with Chronicle's marker-chain
//!    commitment, fails AEAD/RaptorQ authentication, or mismatches the caller's
//!    projection/schema is discarded cleanly and rebuilt from Chronicle without
//!    failing `Database::open`.
//! 2. **ECS-coded (`FGEC` v1)**: content-addressed via `logical_object_id` and
//!    encoded through page-framed XChaCha20-Poly1305 + systematic RFC 6330
//!    RaptorQ (`ExtentObjectKind::IndexSegment`), healing minor symbol bit-rot
//!    deterministically on read.
//! 3. **Bounded `O(|Delta|)` catch-up**: when valid at `watermark_seq <=
//!    committed_head_seq`, only the Chronicle delta suffix
//!    `(watermark_seq..=committed_head_seq]` is replayed (`0` full-graph vertex
//!    or Strata block scans).

use std::path::{Path, PathBuf};

use asupersync::fs::{OpenOptions, Vfs, VfsFile};
use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use fgdb_crypto::zeroize::SharedSecret;
use fgdb_crypto::{Digest, derive_key, keyed_hash, logical_object_id};
use fgdb_delta_types::RelationId;
use fgdb_gql::GqlQueryPolicy;
use fgdb_strata::store::ecs_extent::{
    ExtentError, ExtentObjectKind, StrataEcsConfig, decode_extent, encode_extent,
};
use fgdb_types::context::CommitCx;
use fgdb_types::ids::ObjectId;
use fgdb_types::{CommitSeq, QueryCx};

type BeaconOptions =
    fgdb_beacon::read::ReadOptions<fgdb_delta_types::PropertyKeyId, fgdb_delta_types::LabelId>;
use crate::{
    BRANCH, Database, DatabaseKeys, GRAPH, MaintainedBeaconError, MaintainedBeaconIndex, ReadError,
    RebuildError, RefreshReport, StandingQueryError, StandingQueryHandle, chain_commitment_at,
    sync_vfs_directory,
};
use fgdb_beacon::read::ReadPolicy;

const HEADER_MAGIC: [u8; 4] = *b"FGDC";
const HEADER_VERSION_V1: u16 = 1;
const HEADER_BYTES: usize = 128;
const HEADER_BODY_BYTES: usize = 96;
const MAX_DERIVED_CHECKPOINT_PLAIN_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum CheckpointKind {
    Beacon = 1,
    RippleReachability = 2,
}

impl CheckpointKind {
    const fn subdir(self) -> &'static str {
        match self {
            Self::Beacon => "beacon",
            Self::RippleReachability => "ripple",
        }
    }
}

/// Reason a derived checkpoint was discarded in favor of an authoritative
/// Chronicle rebuild (`FG-INV-18`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DerivedRebuildReason {
    /// No checkpoint existed for this registration name yet.
    Missing,
    /// `watermark_seq > committed_head_seq` (e.g. `manifest.root` or Chronicle
    /// head rolled back past an uncoordinated derived checkpoint).
    AheadOfChronicleHead {
        watermark_seq: CommitSeq,
        head_seq: CommitSeq,
    },
    /// `chain_commitment_at(watermark_seq)` disagreed with the checkpoint's
    /// recorded Chronicle chain commitment.
    ChainMismatch { watermark_seq: CommitSeq },
    /// Checkpoint bytes were truncated/corrupt beyond RaptorQ repair or
    /// mismatched the requested index/query definition.
    CorruptOrIncompatible,
    /// Chronicle delta suffix `(watermark_seq..=head_seq]` could not be
    /// materialized (e.g. explicitly retired prefix).
    DeltaWindowUnavailable { watermark_seq: CommitSeq },
}

/// Outcome of loading a Chronicle-watermarked derived checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DerivedCheckpointOutcome {
    /// Restored the persisted arrangement at `watermark_seq` and replayed only
    /// `(watermark_seq..=head_seq]` deltas with zero full-graph scans.
    CatchUp {
        watermark_seq: CommitSeq,
        head_seq: CommitSeq,
        commits_replayed: u64,
        touched_vertices: usize,
        healed_by_raptorq: bool,
    },
    /// Discarded missing, stale, rolled-back, or corrupt derived state
    /// (`FG-INV-18`) and rebuilt from authoritative Chronicle state.
    RebuiltFromChronicle { reason: DerivedRebuildReason },
}

#[derive(Debug)]
pub enum DerivedCheckpointError {
    EmptyName,
    Read(ReadError),
    Io(std::io::Error),
    Extent(ExtentError),
    Interrupted(Box<asupersync::error::Error>),
    Beacon(MaintainedBeaconError),
    Standing(StandingQueryError),
}

impl core::fmt::Display for DerivedCheckpointError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyName => f.write_str("derived checkpoint name must be non-empty"),
            Self::Read(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
            Self::Extent(error) => error.fmt(f),
            Self::Interrupted(error) => error.fmt(f),
            Self::Beacon(error) => error.fmt(f),
            Self::Standing(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for DerivedCheckpointError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Extent(error) => Some(error),
            Self::Interrupted(error) => Some(error.as_ref()),
            Self::Beacon(error) => Some(error),
            Self::Standing(error) => Some(error),
            Self::EmptyName => None,
        }
    }
}

fn derived_ecs_config(keys: &DatabaseKeys) -> StrataEcsConfig {
    let dek = SharedSecret::new(derive_key("fgdb:derived-ecs:dek:v1", keys.dek.expose()));
    StrataEcsConfig::new(dek).with_coordinates(GRAPH, BRANCH, 1)
}

fn header_mac_key(keys: &DatabaseKeys) -> [u8; 32] {
    derive_key("fgdb:derived-checkpoint:hdr:v1", keys.dek.expose())
}

fn name_file_stem(keys: &DatabaseKeys, kind: CheckpointKind, name: &str) -> String {
    let name_key = derive_key("fgdb:derived-checkpoint:name:v1", keys.k_oid.expose());
    let mut material = Vec::with_capacity(1 + 32 + name.len());
    material.push(kind as u8);
    material.extend_from_slice(&keys.namespace.0);
    material.extend_from_slice(name.as_bytes());
    let digest = keyed_hash(&name_key, &material).0;
    let mut hex = String::with_capacity(32);
    for byte in &digest[..16] {
        use core::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

struct CheckpointPaths {
    derived_root: PathBuf,
    kind_dir: PathBuf,
    final_path: PathBuf,
    staging_path: PathBuf,
}

fn checkpoint_paths(
    db_dir: &Path,
    keys: &DatabaseKeys,
    kind: CheckpointKind,
    name: &str,
) -> CheckpointPaths {
    let derived_root = db_dir.join("derived");
    let kind_dir = derived_root.join(kind.subdir());
    let stem = name_file_stem(keys, kind, name);
    let final_path = kind_dir.join(format!("{stem}.fgec"));
    let staging_path = kind_dir.join(format!("{stem}.staging"));
    CheckpointPaths {
        derived_root,
        kind_dir,
        final_path,
        staging_path,
    }
}

fn encode_checkpoint_container(
    keys: &DatabaseKeys,
    kind: CheckpointKind,
    watermark_seq: CommitSeq,
    watermark_chain_hash: Digest,
    plaintext: &[u8],
) -> Result<Vec<u8>, DerivedCheckpointError> {
    let mut logical_hdr = [0u8; 48];
    logical_hdr[..4].copy_from_slice(&HEADER_MAGIC);
    logical_hdr[4..6].copy_from_slice(&HEADER_VERSION_V1.to_le_bytes());
    logical_hdr[6] = kind as u8;
    logical_hdr[8..16].copy_from_slice(&watermark_seq.0.to_le_bytes());
    logical_hdr[16..48].copy_from_slice(&watermark_chain_hash.0);

    let oid_digest = logical_object_id(
        keys.k_oid.expose(),
        &keys.namespace.0,
        &logical_hdr,
        plaintext,
    );
    let object_id = ObjectId(oid_digest.0);

    let ecs_cfg = derived_ecs_config(keys);
    let extent = encode_extent(
        &ecs_cfg,
        keys.namespace,
        ExtentObjectKind::IndexSegment,
        object_id,
        plaintext,
    )
    .map_err(DerivedCheckpointError::Extent)?;

    let mut header = [0u8; HEADER_BYTES];
    header[..48].copy_from_slice(&logical_hdr);
    header[48..80].copy_from_slice(&object_id.0);
    let mac = keyed_hash(&header_mac_key(keys), &header[..HEADER_BODY_BYTES]).0;
    header[HEADER_BODY_BYTES..HEADER_BYTES].copy_from_slice(&mac);

    let mut out = Vec::with_capacity(HEADER_BYTES + extent.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&extent);
    Ok(out)
}

enum LoadedContainer {
    Valid {
        watermark_seq: CommitSeq,
        plaintext: Vec<u8>,
        healed_by_raptorq: bool,
    },
    Discard(DerivedRebuildReason),
}

fn verify_and_decode_container(
    keys: &DatabaseKeys,
    expected_kind: CheckpointKind,
    committed_head_seq: CommitSeq,
    chain: &fgdb_chronicle::MarkerChain,
    raw: &[u8],
) -> LoadedContainer {
    if raw.len() < HEADER_BYTES {
        return LoadedContainer::Discard(DerivedRebuildReason::CorruptOrIncompatible);
    }
    let header = &raw[..HEADER_BYTES];
    if header[..4] != HEADER_MAGIC {
        return LoadedContainer::Discard(DerivedRebuildReason::CorruptOrIncompatible);
    }
    let expected_mac = keyed_hash(&header_mac_key(keys), &header[..HEADER_BODY_BYTES]).0;
    let mut diff = 0u8;
    for (&a, &b) in expected_mac
        .iter()
        .zip(&header[HEADER_BODY_BYTES..HEADER_BYTES])
    {
        diff |= a ^ b;
    }
    if diff != 0 {
        return LoadedContainer::Discard(DerivedRebuildReason::CorruptOrIncompatible);
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != HEADER_VERSION_V1
        || header[6] != (expected_kind as u8)
        || header[7] != 0
        || header[80..HEADER_BODY_BYTES] != [0u8; 16]
    {
        return LoadedContainer::Discard(DerivedRebuildReason::CorruptOrIncompatible);
    }
    let mut seq_bytes = [0u8; 8];
    seq_bytes.copy_from_slice(&header[8..16]);
    let watermark_seq = CommitSeq(u64::from_le_bytes(seq_bytes));

    if watermark_seq.0 > committed_head_seq.0 {
        return LoadedContainer::Discard(DerivedRebuildReason::AheadOfChronicleHead {
            watermark_seq,
            head_seq: committed_head_seq,
        });
    }

    let mut chain_bytes = [0u8; 32];
    chain_bytes.copy_from_slice(&header[16..48]);
    let saved_chain_hash = Digest(chain_bytes);
    if chain_commitment_at(chain, watermark_seq) != Some(saved_chain_hash) {
        return LoadedContainer::Discard(DerivedRebuildReason::ChainMismatch { watermark_seq });
    }

    let mut oid_bytes = [0u8; 32];
    oid_bytes.copy_from_slice(&header[48..80]);
    let object_id = ObjectId(oid_bytes);

    let ecs_cfg = derived_ecs_config(keys);
    let Ok(decoded) = decode_extent(
        &ecs_cfg,
        keys.namespace,
        ExtentObjectKind::IndexSegment,
        object_id,
        MAX_DERIVED_CHECKPOINT_PLAIN_BYTES,
        &raw[HEADER_BYTES..],
        false,
    ) else {
        return LoadedContainer::Discard(DerivedRebuildReason::CorruptOrIncompatible);
    };

    let recomputed = logical_object_id(
        keys.k_oid.expose(),
        &keys.namespace.0,
        &header[..48],
        &decoded.plaintext,
    );
    if recomputed.0 != object_id.0 {
        return LoadedContainer::Discard(DerivedRebuildReason::CorruptOrIncompatible);
    }

    LoadedContainer::Valid {
        watermark_seq,
        plaintext: decoded.plaintext,
        healed_by_raptorq: decoded.needs_rewrite,
    }
}

async fn write_checkpoint_atomic<V: Vfs>(
    cx: &CommitCx,
    vfs: &V,
    db_dir: &Path,
    paths: &CheckpointPaths,
    bytes: &[u8],
) -> Result<(), DerivedCheckpointError> {
    cx.checkpoint()
        .map_err(DerivedCheckpointError::Interrupted)?;
    cx.with_restriction_async(vfs.create_dir_all(&paths.kind_dir))
        .await
        .map_err(DerivedCheckpointError::Io)?;
    cx.with_restriction_async(async {
        let mut file = vfs
            .open(
                &paths.staging_path,
                &OpenOptions::new().write(true).create(true).truncate(true),
            )
            .await?;
        file.write_all(bytes).await?;
        file.sync_all().await
    })
    .await
    .map_err(DerivedCheckpointError::Io)?;
    cx.with_restriction_async(vfs.rename(&paths.staging_path, &paths.final_path))
        .await
        .map_err(DerivedCheckpointError::Io)?;
    sync_vfs_directory(cx, vfs, &paths.kind_dir)
        .await
        .map_err(DerivedCheckpointError::Io)?;
    sync_vfs_directory(cx, vfs, &paths.derived_root)
        .await
        .map_err(DerivedCheckpointError::Io)?;
    sync_vfs_directory(cx, vfs, db_dir)
        .await
        .map_err(DerivedCheckpointError::Io)?;
    Ok(())
}

enum ReadOutcome {
    Present(Vec<u8>),
    Missing,
    Unreadable,
}

async fn read_checkpoint_optional<V: Vfs>(
    cx: &CommitCx,
    vfs: &V,
    final_path: &Path,
) -> Result<ReadOutcome, DerivedCheckpointError> {
    cx.checkpoint()
        .map_err(DerivedCheckpointError::Interrupted)?;
    let file = match cx
        .with_restriction_async(vfs.open(final_path, &OpenOptions::new().read(true)))
        .await
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(ReadOutcome::Missing),
        Err(_) => return Ok(ReadOutcome::Unreadable),
    };
    let mut buf = Vec::new();
    let mut reader = file;
    match cx
        .with_restriction_async(async { reader.read_to_end(&mut buf).await })
        .await
    {
        Ok(_) => Ok(ReadOutcome::Present(buf)),
        Err(_) => Ok(ReadOutcome::Unreadable),
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Persist a healthy `MaintainedBeaconIndex` as a Chronicle-watermarked,
    /// `FGEC` v1 (XChaCha20-Poly1305 + RaptorQ) derived checkpoint under
    /// `<db_dir>/derived/beacon/`.
    pub async fn save_beacon_checkpoint(
        &self,
        cx: &CommitCx,
        query_cx: &QueryCx,
        handle: &MaintainedBeaconIndex,
        name: &str,
    ) -> Result<CommitSeq, DerivedCheckpointError> {
        if name.is_empty() {
            return Err(DerivedCheckpointError::EmptyName);
        }
        cx.checkpoint()
            .map_err(DerivedCheckpointError::Interrupted)?;
        let (watermark_seq, payload) = self
            .export_beacon_checkpoint_payload(query_cx, handle)
            .map_err(DerivedCheckpointError::Beacon)?;
        let chain_hash =
            chain_commitment_at(self.coordinator.chain(), watermark_seq).ok_or_else(|| {
                DerivedCheckpointError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing Chronicle chain commitment at current Beacon frontier",
                ))
            })?;
        let container = encode_checkpoint_container(
            &self.keys,
            CheckpointKind::Beacon,
            watermark_seq,
            chain_hash,
            &payload,
        )?;
        let db_dir = self.coordinator.database_dir();
        let paths = checkpoint_paths(db_dir, &self.keys, CheckpointKind::Beacon, name);
        write_checkpoint_atomic(cx, &self.vfs, db_dir, &paths, &container).await?;
        Ok(watermark_seq)
    }

    /// Rehydrate a Chronicle-watermarked `MaintainedBeaconIndex` from
    /// `<db_dir>/derived/beacon/` and catch up `(watermark_seq..=head_seq]` in
    /// `O(|Delta|)` with zero full-graph vertex scans, or discard stale/corrupt
    /// derived bytes (`FG-INV-18`) and rebuild from Chronicle.
    pub async fn load_or_rebuild_beacon_checkpoint(
        &mut self,
        cx: &CommitCx,
        query_cx: &QueryCx,
        name: &str,
        options: &BeaconOptions,
        maintenance: ReadPolicy,
    ) -> Result<(MaintainedBeaconIndex, DerivedCheckpointOutcome), DerivedCheckpointError> {
        if name.is_empty() {
            return Err(DerivedCheckpointError::EmptyName);
        }
        cx.checkpoint()
            .map_err(DerivedCheckpointError::Interrupted)?;
        query_cx
            .checkpoint()
            .map_err(DerivedCheckpointError::Interrupted)?;
        let head_seq = self.frontier().map_err(DerivedCheckpointError::Read)?;
        if let Some(requested) = options.as_of
            && requested != head_seq
        {
            return Err(DerivedCheckpointError::Beacon(
                MaintainedBeaconError::NotCurrent {
                    requested,
                    current: head_seq,
                },
            ));
        }

        let db_dir = self.coordinator.database_dir().to_path_buf();
        let paths = checkpoint_paths(&db_dir, &self.keys, CheckpointKind::Beacon, name);

        let discard_reason = match read_checkpoint_optional(cx, &self.vfs, &paths.final_path)
            .await?
        {
            ReadOutcome::Missing => DerivedRebuildReason::Missing,
            ReadOutcome::Unreadable => DerivedRebuildReason::CorruptOrIncompatible,
            ReadOutcome::Present(raw) => match verify_and_decode_container(
                &self.keys,
                CheckpointKind::Beacon,
                head_seq,
                self.coordinator.chain(),
                &raw,
            ) {
                LoadedContainer::Discard(reason) => reason,
                LoadedContainer::Valid {
                    watermark_seq,
                    plaintext,
                    healed_by_raptorq,
                } => {
                    match crate::ResidentIndex::decode_derived_payload(
                        std::sync::Arc::clone(&self.handle_owner),
                        options,
                        watermark_seq,
                        &plaintext,
                        query_cx,
                    ) {
                        Err(err) => {
                            return Err(DerivedCheckpointError::Beacon(
                                MaintainedBeaconError::Resident(err),
                            ));
                        }
                        Ok(None) => DerivedRebuildReason::CorruptOrIncompatible,
                        Ok(Some(mut resident)) => {
                            if watermark_seq < head_seq
                                && let Err(err) = self.ensure_delta_window(cx, watermark_seq).await
                            {
                                match err {
                                    RebuildError::Interrupted(cancel) => {
                                        return Err(DerivedCheckpointError::Interrupted(cancel));
                                    }
                                    _ => {
                                        let rebuilt = self
                                            .register_beacon_index(query_cx, options, maintenance)
                                            .map_err(DerivedCheckpointError::Beacon)?;
                                        return Ok((
                                            rebuilt,
                                            DerivedCheckpointOutcome::RebuiltFromChronicle {
                                                reason:
                                                    DerivedRebuildReason::DeltaWindowUnavailable {
                                                        watermark_seq,
                                                    },
                                            },
                                        ));
                                    }
                                }
                            }
                            let refresh_report = if watermark_seq == head_seq {
                                RefreshReport {
                                    from: head_seq,
                                    through: head_seq,
                                    commits: 0,
                                    touched_vertices: 0,
                                }
                            } else {
                                match resident.refresh(query_cx, self, Some(head_seq), maintenance)
                                {
                                    Ok(report) => report,
                                    Err(
                                        crate::ResidentIndexError::UnsupportedDelta
                                        | crate::ResidentIndexError::IncompleteDelta { .. }
                                        | crate::ResidentIndexError::Source(
                                            ReadError::DeltaCursorRetired { .. },
                                        ),
                                    ) => {
                                        let rebuilt = self
                                            .register_beacon_index(query_cx, options, maintenance)
                                            .map_err(DerivedCheckpointError::Beacon)?;
                                        return Ok((
                                            rebuilt,
                                            DerivedCheckpointOutcome::RebuiltFromChronicle {
                                                reason:
                                                    DerivedRebuildReason::DeltaWindowUnavailable {
                                                        watermark_seq,
                                                    },
                                            },
                                        ));
                                    }
                                    Err(err) => {
                                        return Err(DerivedCheckpointError::Beacon(
                                            MaintainedBeaconError::Resident(err),
                                        ));
                                    }
                                }
                            };
                            let handle = self
                                .install_restored_beacon_index(
                                    query_cx,
                                    resident,
                                    maintenance,
                                    refresh_report,
                                )
                                .map_err(DerivedCheckpointError::Beacon)?;
                            return Ok((
                                handle,
                                DerivedCheckpointOutcome::CatchUp {
                                    watermark_seq,
                                    head_seq,
                                    commits_replayed: refresh_report.commits,
                                    touched_vertices: refresh_report.touched_vertices,
                                    healed_by_raptorq,
                                },
                            ));
                        }
                    }
                }
            },
        };

        let rebuilt = self
            .register_beacon_index(query_cx, options, maintenance)
            .map_err(DerivedCheckpointError::Beacon)?;
        Ok((
            rebuilt,
            DerivedCheckpointOutcome::RebuiltFromChronicle {
                reason: discard_reason,
            },
        ))
    }

    /// Persist a healthy Ripple standing-reachability Z-set arrangement as a
    /// Chronicle-watermarked, `FGEC` v1 (XChaCha20-Poly1305 + RaptorQ) derived
    /// checkpoint under `<db_dir>/derived/ripple/`.
    pub async fn save_standing_reachability_checkpoint(
        &self,
        cx: &CommitCx,
        query_cx: &QueryCx,
        handle: &StandingQueryHandle,
        name: &str,
    ) -> Result<CommitSeq, DerivedCheckpointError> {
        if name.is_empty() {
            return Err(DerivedCheckpointError::EmptyName);
        }
        cx.checkpoint()
            .map_err(DerivedCheckpointError::Interrupted)?;
        let (_relation, watermark_seq, payload) = self
            .export_standing_reachability_checkpoint(query_cx, handle)
            .map_err(DerivedCheckpointError::Standing)?;
        let chain_hash =
            chain_commitment_at(self.coordinator.chain(), watermark_seq).ok_or_else(|| {
                DerivedCheckpointError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing Chronicle chain commitment at current standing frontier",
                ))
            })?;
        let container = encode_checkpoint_container(
            &self.keys,
            CheckpointKind::RippleReachability,
            watermark_seq,
            chain_hash,
            &payload,
        )?;
        let db_dir = self.coordinator.database_dir();
        let paths = checkpoint_paths(db_dir, &self.keys, CheckpointKind::RippleReachability, name);
        write_checkpoint_atomic(cx, &self.vfs, db_dir, &paths, &container).await?;
        Ok(watermark_seq)
    }

    /// Rehydrate a Chronicle-watermarked Ripple standing-reachability Z-set
    /// arrangement from `<db_dir>/derived/ripple/` and catch up
    /// `(watermark_seq..=head_seq]` in `O(|Delta|)` with zero Strata block
    /// scans, or discard stale/corrupt derived bytes (`FG-INV-18`) and rebuild
    /// from Chronicle.
    pub async fn load_or_rebuild_standing_reachability_checkpoint(
        &mut self,
        cx: &CommitCx,
        query_cx: &QueryCx,
        name: &str,
        relation: RelationId,
        policy: GqlQueryPolicy,
    ) -> Result<(StandingQueryHandle, DerivedCheckpointOutcome), DerivedCheckpointError> {
        if name.is_empty() {
            return Err(DerivedCheckpointError::EmptyName);
        }
        cx.checkpoint()
            .map_err(DerivedCheckpointError::Interrupted)?;
        query_cx
            .checkpoint()
            .map_err(DerivedCheckpointError::Interrupted)?;
        let head_seq = self.frontier().map_err(DerivedCheckpointError::Read)?;

        let db_dir = self.coordinator.database_dir().to_path_buf();
        let paths = checkpoint_paths(
            &db_dir,
            &self.keys,
            CheckpointKind::RippleReachability,
            name,
        );

        let discard_reason =
            match read_checkpoint_optional(cx, &self.vfs, &paths.final_path).await? {
                ReadOutcome::Missing => DerivedRebuildReason::Missing,
                ReadOutcome::Unreadable => DerivedRebuildReason::CorruptOrIncompatible,
                ReadOutcome::Present(raw) => match verify_and_decode_container(
                    &self.keys,
                    CheckpointKind::RippleReachability,
                    head_seq,
                    self.coordinator.chain(),
                    &raw,
                ) {
                    LoadedContainer::Discard(reason) => reason,
                    LoadedContainer::Valid {
                        watermark_seq,
                        plaintext,
                        healed_by_raptorq,
                    } => {
                        if let Err(err) = self.ensure_delta_window(cx, watermark_seq).await {
                            match err {
                                RebuildError::Interrupted(cancel) => {
                                    return Err(DerivedCheckpointError::Interrupted(cancel));
                                }
                                _ => {
                                    let rebuilt = self
                                        .register_standing_reachability(query_cx, relation, policy)
                                        .map_err(DerivedCheckpointError::Standing)?;
                                    return Ok((
                                        rebuilt,
                                        DerivedCheckpointOutcome::RebuiltFromChronicle {
                                            reason: DerivedRebuildReason::DeltaWindowUnavailable {
                                                watermark_seq,
                                            },
                                        },
                                    ));
                                }
                            }
                        }
                        match self.catch_up_or_none_standing_reachability(
                            query_cx,
                            &plaintext,
                            watermark_seq,
                            relation,
                            policy,
                        ) {
                            Err(err) => return Err(DerivedCheckpointError::Standing(err)),
                            Ok(None) => DerivedRebuildReason::CorruptOrIncompatible,
                            Ok(Some((state, commits_replayed))) => {
                                let handle = self.install_restored_standing_reachability(state);
                                return Ok((
                                    handle,
                                    DerivedCheckpointOutcome::CatchUp {
                                        watermark_seq,
                                        head_seq,
                                        commits_replayed,
                                        touched_vertices: 0,
                                        healed_by_raptorq,
                                    },
                                ));
                            }
                        }
                    }
                },
            };

        let rebuilt = self
            .register_standing_reachability(query_cx, relation, policy)
            .map_err(DerivedCheckpointError::Standing)?;
        Ok((
            rebuilt,
            DerivedCheckpointOutcome::RebuiltFromChronicle {
                reason: discard_reason,
            },
        ))
    }

    /// Deterministic relative path of a named Beacon checkpoint inside the
    /// database directory (exposed for scrubbing and fault-injection tests).
    #[must_use]
    pub fn beacon_checkpoint_relative_path(&self, name: &str) -> PathBuf {
        checkpoint_paths(Path::new(""), &self.keys, CheckpointKind::Beacon, name).final_path
    }

    /// Deterministic relative path of a named standing-reachability checkpoint
    /// inside the database directory (exposed for scrubbing and fault-injection
    /// tests).
    #[must_use]
    pub fn standing_reachability_checkpoint_relative_path(&self, name: &str) -> PathBuf {
        checkpoint_paths(
            Path::new(""),
            &self.keys,
            CheckpointKind::RippleReachability,
            name,
        )
        .final_path
    }
}
