//! Commit packs: many immutable store objects in one durable file (fgdb-79kyi).
//!
//! A publication batch that stages at least [`PACK_MIN_OBJECTS`] objects
//! writes them as ONE pack instead of one file each, so a bulk commit costs
//! one inode sync, one read-back and one rename instead of one of each per
//! touched family. On Apple platforms every sync is an `F_FULLFSYNC`: a
//! 456k-row load wrote 32,478 objects and spent 86 s of its 79 s wall in the
//! kernel. Reads benefit the same way: an open walks every object, and a pack
//! is read whole once instead of a few hundred files one at a time.
//!
//! **THE PACK IS A LOCATION, NOT AN AUTHORITY.** Every object keeps its
//! content identity, and every read path still re-derives it from the bytes it
//! got. A pack's name and table only say where to look, exactly as a loose
//! object's path does (doctrine 5). A damaged table can make an object
//! unfindable or point at the wrong bytes; both surface as the same lost or
//! mismatched object a damaged loose file would.
//!
//! Layout (version 1), little-endian:
//!
//! ```text
//! magic "FGPK" | version u16 | reserved u16 (0) | count u32
//! count x { id [u8; 32] | offset u64 | len u32 }
//! the objects' bytes, in table order, back to back to the end of the file
//! ```
//!
//! A table is admitted only in that exact canonical shape: the first object
//! starts right after the table, each starts where the previous ended, and the
//! last ends at the end of the file. A pack is named by the BLAKE3 hash of its
//! complete bytes, so two batches never publish different bytes under one name.
//!
//! Small batches (an ordinary transaction's handful of objects) stay loose:
//! their sync count is already small, and loose files are what the existing
//! crash and damage tooling addresses by path.

use super::{StoreError, read_bounded};
use asupersync::fs::{Vfs, VfsFile};
use asupersync::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use fgdb_types::StorageReadCx;
use fgdb_types::ids::ObjectId;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

/// Subdirectory of the block directory holding packs.
pub const PACK_DIR: &str = "packs";

/// The noncanonical name a pack is written under before it is synced, read
/// back and renamed. Only the publication-permit owner touches it.
pub(super) const PACK_STAGING_FILE: &str = ".pack-staging";

const PACK_MAGIC: [u8; 4] = *b"FGPK";
const PACK_VERSION: u16 = 1;
const HEADER_BYTES: usize = 12;
const ENTRY_BYTES: usize = 44;

/// The fewest objects one batch flush packs. Below it, objects stay loose.
pub const PACK_MIN_OBJECTS: usize = 64;

/// A batch flushes once its unflushed objects reach this many bytes, so one
/// pack (and the memory holding a batch's staged bytes) stays bounded.
pub(super) const PACK_TARGET_BYTES: usize = 64 << 20;

/// The largest pack this store reads. Writers stay far below it: a flush
/// starts at [`PACK_TARGET_BYTES`] and adds at most one object.
pub(super) const MAX_PACK_BYTES: u64 = 256 << 20;

/// The most entries a pack table may declare. Even at the byte ceiling a
/// table this long leaves no room for payload; it only bounds the allocation
/// a damaged count could request.
const MAX_PACK_OBJECTS: u64 = MAX_PACK_BYTES / ENTRY_BYTES as u64;

/// Bytes of whole packs kept resident between reads. An open walks every
/// object once, in root order, which visits each commit's pack once per
/// family; a resident pack serves all of those visits from one read. Packs
/// are admitted until the budget is spent and never evicted: once it is,
/// other packs are read one object at a time. Evicting instead would make a
/// walk over more pack bytes than the budget reload a whole pack for almost
/// every object it reads.
const PACK_CACHE_BYTES: usize = 256 << 20;

/// Where one packed object's bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Location {
    pack: u32,
    offset: u64,
    len: u32,
}

impl Location {
    pub(super) fn len(self) -> u64 {
        u64::from(self.len)
    }
}

/// The copies of one object this index knows, newest first.
pub(super) enum Located {
    None,
    One(Location),
    Several(Vec<Location>),
}

/// Encode `objects` as one pack. The caller never repeats an identity.
pub(super) fn encode(objects: &[(ObjectId, &[u8])]) -> Vec<u8> {
    let table_end = HEADER_BYTES + objects.len() * ENTRY_BYTES;
    let payload: usize = objects.iter().map(|(_, bytes)| bytes.len()).sum();
    let mut out = Vec::with_capacity(table_end + payload);
    out.extend_from_slice(&PACK_MAGIC);
    out.extend_from_slice(&PACK_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(objects.len())
            .expect("a batch flushes far below u32::MAX objects")
            .to_le_bytes(),
    );
    let mut offset = table_end as u64;
    for (id, bytes) in objects {
        out.extend_from_slice(&id.0);
        out.extend_from_slice(&offset.to_le_bytes());
        let len = u32::try_from(bytes.len()).expect("a stored object is far below 4 GiB");
        out.extend_from_slice(&len.to_le_bytes());
        offset += u64::from(len);
    }
    for (_, bytes) in objects {
        out.extend_from_slice(bytes);
    }
    out
}

/// The canonical file name of a pack's complete bytes.
pub(super) fn name(bytes: &[u8]) -> String {
    format!("{}.pack", fgdb_crypto::blake3::hash(bytes).to_hex())
}

/// The declared object count, after checking the fixed header.
fn header_count(header: &[u8], file_len: u64) -> Result<u64, &'static str> {
    let header: &[u8; HEADER_BYTES] = header.try_into().map_err(|_| "short pack header")?;
    if header[..4] != PACK_MAGIC {
        return Err("not a pack");
    }
    if u16::from_le_bytes([header[4], header[5]]) != PACK_VERSION
        || u16::from_le_bytes([header[6], header[7]]) != 0
    {
        return Err("unknown pack version");
    }
    let count = u64::from(u32::from_le_bytes([
        header[8], header[9], header[10], header[11],
    ]));
    if count > MAX_PACK_OBJECTS
        || HEADER_BYTES as u64 + count * ENTRY_BYTES as u64 > file_len
        || file_len > MAX_PACK_BYTES
    {
        return Err("pack table exceeds its file");
    }
    Ok(count)
}

/// Decode and admit a pack table, `table` being the bytes right after the
/// header. Refuses every shape but the canonical one [`encode`] writes.
fn decode_table(table: &[u8], file_len: u64) -> Result<Vec<(ObjectId, u64, u32)>, &'static str> {
    let (rows, torn) = table.as_chunks::<ENTRY_BYTES>();
    if !torn.is_empty() {
        return Err("torn pack table");
    }
    let mut next = (HEADER_BYTES + table.len()) as u64;
    let mut entries = Vec::with_capacity(rows.len());
    for entry in rows {
        let id = ObjectId(entry[..32].try_into().expect("32-byte identity"));
        let offset = u64::from_le_bytes(entry[32..40].try_into().expect("8-byte offset"));
        let len = u32::from_le_bytes(entry[40..44].try_into().expect("4-byte length"));
        if offset != next {
            return Err("pack objects are not back to back");
        }
        next = offset
            .checked_add(u64::from(len))
            .ok_or("pack object overflows")?;
        if next > file_len {
            return Err("pack object runs past its file");
        }
        entries.push((id, offset, len));
    }
    if next != file_len {
        return Err("pack has bytes no table entry names");
    }
    Ok(entries)
}

enum Slot {
    Empty,
    Loading,
    Loaded(Arc<[u8]>),
    /// Served as many objects as its table holds, then released: a walk is
    /// done with it. Later reads take one object at a time.
    Drained,
}

struct PackFile {
    path: PathBuf,
    len: u64,
    objects: u64,
    /// Objects this pack has served by any path: whole, or one at a time
    /// while another read was loading it.
    served: std::sync::atomic::AtomicU64,
    slot: Mutex<Slot>,
}

#[derive(Default)]
struct Table {
    packs: Vec<Arc<PackFile>>,
    /// Every pack file name this index has examined, admitted or not, so a
    /// refresh reads each table once.
    seen: HashSet<OsString>,
    objects: HashMap<ObjectId, Location>,
    /// Older copies of an object some newer pack also holds.
    older: HashMap<ObjectId, Vec<Location>>,
}

/// The packs of one block directory and where each packed object is.
/// Shared by every clone of a store; packs are immutable once named.
pub(super) struct PackIndex {
    dir: PathBuf,
    table: RwLock<Table>,
    /// Bytes of whole packs admitted as resident, never more than
    /// [`PACK_CACHE_BYTES`]. A pack being loaded is counted from the start.
    resident: Mutex<usize>,
}

impl PackIndex {
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Index every pack already in `dir`.
    pub(super) async fn load<V: Vfs>(
        cx: &impl StorageReadCx,
        vfs: &V,
        dir: PathBuf,
    ) -> Result<Self, StoreError> {
        let index = Self {
            dir,
            table: RwLock::new(Table::default()),
            resident: Mutex::new(0),
        };
        index.refresh(cx, vfs).await?;
        Ok(index)
    }

    /// Read the tables of packs this index has not examined yet: another
    /// process may have published since this store was opened. Returns
    /// whether any object location was added. A pack whose table is not
    /// admitted is skipped; the objects only it holds read as absent.
    pub(super) async fn refresh<V: Vfs>(
        &self,
        cx: &impl StorageReadCx,
        vfs: &V,
    ) -> Result<bool, StoreError> {
        let mut entries = match cx.with_restriction_async(vfs.read_dir(&self.dir)).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let mut added = false;
        while let Some(entry) = cx.with_restriction_async(entries.next_entry()).await? {
            let file_name = entry.file_name();
            if !Path::new(&file_name)
                .extension()
                .is_some_and(|extension| extension == "pack")
                || self
                    .table
                    .read()
                    .expect("pack table")
                    .seen
                    .contains(&file_name)
            {
                continue;
            }
            let path = entry.path();
            let admitted = self.read_table(cx, vfs, &path).await?;
            let mut table = self.table.write().expect("pack table");
            if !table.seen.insert(file_name) {
                continue;
            }
            if let Some((len, objects)) = admitted {
                added |= !objects.is_empty();
                Self::insert(&mut table, path, len, objects);
            }
        }
        Ok(added)
    }

    /// The admitted table of one pack file, or `None` when it is not one.
    async fn read_table<V: Vfs>(
        &self,
        cx: &impl StorageReadCx,
        vfs: &V,
        path: &Path,
    ) -> Result<Option<(u64, Vec<(ObjectId, u64, u32)>)>, StoreError> {
        cx.with_restriction_async(async {
            let mut file = vfs.open_read(path).await?;
            let len = file.metadata().await?.len();
            let mut header = [0u8; HEADER_BYTES];
            if len < HEADER_BYTES as u64 {
                return Ok(None);
            }
            file.read_exact(&mut header).await?;
            let Ok(count) = header_count(&header, len) else {
                return Ok(None);
            };
            let mut table = vec![0u8; usize::try_from(count).expect("bounded count") * ENTRY_BYTES];
            file.read_exact(&mut table).await?;
            Ok(decode_table(&table, len).ok().map(|objects| (len, objects)))
        })
        .await
    }

    fn insert(table: &mut Table, path: PathBuf, len: u64, objects: Vec<(ObjectId, u64, u32)>) {
        let pack = u32::try_from(table.packs.len()).expect("fewer than u32::MAX packs");
        table.packs.push(Arc::new(PackFile {
            path,
            len,
            objects: objects.len() as u64,
            served: std::sync::atomic::AtomicU64::new(0),
            slot: Mutex::new(Slot::Empty),
        }));
        for (id, offset, len) in objects {
            let location = Location { pack, offset, len };
            if let Some(previous) = table.objects.insert(id, location) {
                table.older.entry(id).or_default().insert(0, previous);
            }
        }
    }

    /// Register a pack this process just published under `path`.
    pub(super) fn published(&self, path: PathBuf, bytes: &[u8]) {
        let len = bytes.len() as u64;
        let count = header_count(&bytes[..HEADER_BYTES], len).expect("a pack this store encoded");
        let objects = decode_table(
            &bytes[HEADER_BYTES..HEADER_BYTES + count as usize * ENTRY_BYTES],
            len,
        )
        .expect("a pack this store encoded");
        let file_name = path
            .file_name()
            .expect("a pack path names a file")
            .to_os_string();
        let mut table = self.table.write().expect("pack table");
        if table.seen.insert(file_name) {
            Self::insert(&mut table, path, len, objects);
        }
    }

    /// Every packed copy of `id`, newest first.
    pub(super) fn locate(&self, id: ObjectId) -> Located {
        let table = self.table.read().expect("pack table");
        let Some(&newest) = table.objects.get(&id) else {
            return Located::None;
        };
        match table.older.get(&id) {
            None => Located::One(newest),
            Some(older) => {
                let mut all = Vec::with_capacity(1 + older.len());
                all.push(newest);
                all.extend_from_slice(older);
                Located::Several(all)
            }
        }
    }

    /// The bytes at `location`, refusing an object over `limit` before any
    /// read. The caller verifies the object's identity, as for a loose read.
    pub(super) async fn read<V: Vfs>(
        &self,
        cx: &impl StorageReadCx,
        vfs: &V,
        location: Location,
        limit: u64,
    ) -> Result<Vec<u8>, StoreError> {
        super::ensure_size_within_limit(location.len(), limit)?;
        let pack =
            Arc::clone(&self.table.read().expect("pack table").packs[location.pack as usize]);
        let start = usize::try_from(location.offset).expect("pack offsets fit memory");
        let end = start + location.len as usize;
        let size = usize::try_from(pack.len).unwrap_or(usize::MAX);
        let loading = {
            let mut slot = pack.slot.lock().expect("pack slot");
            match &*slot {
                Slot::Loaded(bytes) => {
                    let object = bytes[start..end].to_vec();
                    drop(slot);
                    self.served(&pack);
                    return Ok(object);
                }
                Slot::Loading | Slot::Drained => false,
                Slot::Empty => {
                    let mut resident = self.resident.lock().expect("resident packs");
                    let admitted = resident
                        .checked_add(size)
                        .is_some_and(|total| total <= PACK_CACHE_BYTES);
                    if admitted {
                        *resident += size;
                        *slot = Slot::Loading;
                    }
                    admitted
                }
            }
        };
        if !loading {
            // Another read is loading this pack, a walk already drained it,
            // or the resident budget is spent: read just this object.
            let object = cx
                .with_restriction_async(async {
                    let mut file = vfs.open_read(&pack.path).await?;
                    file.seek(SeekFrom::Start(location.offset)).await?;
                    let mut bytes = vec![0u8; location.len as usize];
                    file.read_exact(&mut bytes).await?;
                    Ok::<_, std::io::Error>(bytes)
                })
                .await
                .map_err(StoreError::Io)?;
            self.served(&pack);
            return Ok(object);
        }
        let loaded = Self::read_whole(cx, vfs, &pack).await;
        let mut slot = pack.slot.lock().expect("pack slot");
        match loaded {
            Ok(bytes) => {
                let object = bytes[start..end].to_vec();
                *slot = Slot::Loaded(bytes);
                drop(slot);
                self.served(&pack);
                Ok(object)
            }
            Err(error) => {
                *slot = Slot::Empty;
                drop(slot);
                *self.resident.lock().expect("resident packs") -= size;
                Err(error)
            }
        }
    }

    /// Count one object served. Once a pack has served as many objects as
    /// its table holds, a loaded pack is released with its budget.
    fn served(&self, pack: &PackFile) {
        let served = pack
            .served
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if served < pack.objects {
            return;
        }
        let mut slot = pack.slot.lock().expect("pack slot");
        if matches!(*slot, Slot::Loaded(_)) {
            *slot = Slot::Drained;
            drop(slot);
            *self.resident.lock().expect("resident packs") -=
                usize::try_from(pack.len).unwrap_or(usize::MAX);
        }
    }

    /// Spend the whole resident budget, so every later read of a pack not
    /// yet resident takes the one-object path.
    #[cfg(test)]
    pub(super) fn spend_resident_budget(&self) {
        *self.resident.lock().expect("resident packs") = PACK_CACHE_BYTES;
    }

    /// How many packs are loaded whole.
    #[cfg(test)]
    pub(super) fn resident_packs(&self) -> usize {
        self.table
            .read()
            .expect("pack table")
            .packs
            .iter()
            .filter(|pack| matches!(*pack.slot.lock().expect("pack slot"), Slot::Loaded(_)))
            .count()
    }

    async fn read_whole<V: Vfs>(
        cx: &impl StorageReadCx,
        vfs: &V,
        pack: &PackFile,
    ) -> Result<Arc<[u8]>, StoreError> {
        let file = cx.with_restriction_async(vfs.open_read(&pack.path)).await?;
        let bytes = read_bounded(cx, file, MAX_PACK_BYTES).await?;
        if bytes.len() as u64 != pack.len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "an immutable pack changed length",
            )
            .into());
        }
        Ok(Arc::from(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> ObjectId {
        ObjectId([byte; 32])
    }

    fn table_of(bytes: &[u8]) -> Result<Vec<(ObjectId, u64, u32)>, &'static str> {
        let count = header_count(&bytes[..HEADER_BYTES], bytes.len() as u64)?;
        decode_table(
            &bytes[HEADER_BYTES..HEADER_BYTES + count as usize * ENTRY_BYTES],
            bytes.len() as u64,
        )
    }

    #[test]
    fn a_pack_table_locates_every_object_back_to_back() {
        let objects: Vec<(ObjectId, Vec<u8>)> = vec![
            (id(1), b"first".to_vec()),
            (id(2), Vec::new()),
            (id(3), vec![7u8; 300]),
        ];
        let refs: Vec<(ObjectId, &[u8])> = objects
            .iter()
            .map(|(id, bytes)| (*id, bytes.as_slice()))
            .collect();
        let pack = encode(&refs);
        let table = table_of(&pack).unwrap();
        assert_eq!(table.len(), 3);
        for ((id, offset, len), (expected_id, bytes)) in table.iter().zip(&objects) {
            assert_eq!(id, expected_id);
            let start = *offset as usize;
            assert_eq!(&pack[start..start + *len as usize], bytes.as_slice());
        }
        assert_eq!(name(&pack), name(&pack.clone()));
        assert!(name(&pack).ends_with(".pack"));
    }

    #[test]
    fn only_the_canonical_table_shape_is_admitted() {
        let pack = encode(&[(id(1), b"abc"), (id(2), b"defg")]);
        let len = pack.len() as u64;
        let mut wrong_magic = pack.clone();
        wrong_magic[0] = b'X';
        assert_eq!(table_of(&wrong_magic), Err("not a pack"));
        let mut wrong_version = pack.clone();
        wrong_version[4] = 2;
        assert_eq!(table_of(&wrong_version), Err("unknown pack version"));
        let mut too_many = pack.clone();
        too_many[8] = 9;
        assert_eq!(table_of(&too_many), Err("pack table exceeds its file"));
        // The second object no longer starts where the first ends.
        let mut gap = pack.clone();
        let second_offset = HEADER_BYTES + ENTRY_BYTES + 32;
        gap[second_offset] += 1;
        assert_eq!(table_of(&gap), Err("pack objects are not back to back"));
        // A trailing byte no entry names, and a truncated last object.
        let mut trailing = pack.clone();
        trailing.push(0);
        assert_eq!(
            table_of(&trailing),
            Err("pack has bytes no table entry names")
        );
        let truncated = &pack[..pack.len() - 1];
        assert_eq!(table_of(truncated), Err("pack object runs past its file"));
        assert!(table_of(&pack).is_ok_and(|table| table.len() == 2) && len > 0);
    }

    #[test]
    fn a_newer_copy_is_located_first_and_older_ones_are_kept() {
        let mut table = Table::default();
        PackIndex::insert(
            &mut table,
            PathBuf::from("a.pack"),
            100,
            vec![(id(1), 56, 4)],
        );
        PackIndex::insert(
            &mut table,
            PathBuf::from("b.pack"),
            100,
            vec![(id(1), 56, 4), (id(2), 60, 4)],
        );
        let index = PackIndex {
            dir: PathBuf::new(),
            table: RwLock::new(table),
            resident: Mutex::new(0),
        };
        match index.locate(id(1)) {
            Located::Several(all) => {
                assert_eq!(
                    all.iter().map(|location| location.pack).collect::<Vec<_>>(),
                    [1, 0]
                );
            }
            _ => panic!("two copies"),
        }
        assert!(matches!(
            index.locate(id(2)),
            Located::One(Location { pack: 1, .. })
        ));
        assert!(matches!(index.locate(id(3)), Located::None));
    }
}
