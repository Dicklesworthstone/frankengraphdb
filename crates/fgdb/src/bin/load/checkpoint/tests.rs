use super::*;
use super::super::{JsonParser, Saved};
use fgdb_types::{EId, VId};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

fn location() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!("fgdb-checkpoint-{}-{}",
        std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir(&path).unwrap();
    path.join("checkpoint")
}
fn saved(rows: usize) -> Saved {
    let chunks = rows.div_ceil(2);
    Saved {
        checkpoint: BulkLoadCheckpoint {
            vertices: (0..rows).map(|i| (format!("v{i}"), VId(i as u128 + 1))).collect(),
            edges: BTreeMap::new(), next_row: rows, frontier: CommitSeq(7 + chunks as u64),
            committed_chunks: chunks,
        },
        base: CommitSeq(7), base_marker: "ab".repeat(32), source_hash: "cd".repeat(32),
        rows_per_chunk: 2,
    }
}
fn limits(saved: &Saved) -> Limits {
    Limits::new(saved.checkpoint.next_row,
        saved.checkpoint.vertices.keys().chain(saved.checkpoint.edges.keys()).map(String::len).sum(),
        1024).unwrap()
}

// The previous production Saved::encode expression is retained only as an
// independent compatibility oracle, not used by live persistence or recovery.
fn legacy(saved: &Saved) -> Vec<u8> {
    let cp = &saved.checkpoint;
    let vertices = cp.vertices.iter().map(|(k, v)| format!("{}:{}", quoted(k), quoted(&v.0.to_string())))
        .collect::<Vec<_>>().join(",");
    let edges = cp.edges.iter().map(|(k, v)| format!("{}:{}", quoted(k), quoted(&v.0.to_string())))
        .collect::<Vec<_>>().join(",");
    format!("{{\"v\":1,\"vertices\":{{{vertices}}},\"edges\":{{{edges}}},\"next_row\":{},\"frontier\":{},\"committed_chunks\":{},\"base_frontier\":{},\"base_marker\":{},\"source_hash\":{},\"rows_per_chunk\":{}}}\n",
        cp.next_row, cp.frontier.0, cp.committed_chunks, saved.base.0,
        quoted(&saved.base_marker), quoted(&saved.source_hash), saved.rows_per_chunk).into_bytes()
}
fn encode(saved: &Saved, limits: Limits) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut control = || Ok(());
    let mut writer = Buffer::new(&mut bytes, &mut control, limits.bytes)?;
    saved.view().write_to(&mut writer, limits)?;
    writer.flush()?;
    drop(writer);
    Ok(bytes)
}

#[test]
fn streamed_v1_encoding_matches_legacy_with_full_width_ids_and_escaped_keys() {
    for n in [0, 1, 2, 17] {
        let value = saved(n);
        let bounds = limits(&value);
        let expected = legacy(&value);
        assert_eq!(encode(&value, bounds).unwrap(), expected);
        let decoded = Saved::decode(std::str::from_utf8(&expected).unwrap(), bounds).unwrap();
        assert_eq!(legacy(&decoded), expected);
    }
    let mut value = saved(3);
    value.checkpoint.vertices = [
        ("\0\n\r\t\"\\λ💡".into(), VId(u128::MAX)),
        ("z".into(), VId(1)),
    ].into();
    value.checkpoint.edges = [("edge\u{07}".into(), EId(u128::MAX - 1))].into();
    let bounds = limits(&value);
    let expected = legacy(&value);
    assert!(std::str::from_utf8(&expected).unwrap().contains("340282366920938463463374607431768211455"));
    assert_eq!(encode(&value, bounds).unwrap(), expected);
    let decoded = Saved::decode(std::str::from_utf8(&expected).unwrap(), bounds).unwrap();
    assert_eq!(decoded.checkpoint.vertices, value.checkpoint.vertices);
    assert_eq!(decoded.checkpoint.edges, value.checkpoint.edges);
    assert_eq!(encode(&value, Limits { bytes: expected.len(), ..bounds }).unwrap(), expected);
    assert!(encode(&value, Limits { bytes: expected.len() - 1, ..bounds }).is_err());
}

#[test]
fn decode_limits_apply_before_large_strings_numbers_and_value_trees_are_retained() {
    assert!(JsonParser::parse_limited("[0,1]", 3, 1).is_ok());
    assert!(JsonParser::parse_limited("[0,1]", 2, 1).unwrap_err().contains("value limit"));
    assert!(JsonParser::parse_limited("\"λ\"", 1, 2).is_ok());
    assert!(JsonParser::parse_limited("\"λ\"", 1, 1).unwrap_err().contains("string limit"));
    assert!(JsonParser::parse_limited("\"\\uD83D\\uDCA1\"", 1, 4).is_ok());
    assert!(JsonParser::parse_limited("\"\\uD83D\\uDCA1\"", 1, 3).unwrap_err().contains("string limit"));
    assert!(JsonParser::parse_limited("123", 1, 2).unwrap_err().contains("number limit"));
    // Refuse even when the byte allowance is generous: it is not a replacement
    // for a node count, decoded UTF-8 length or checkpoint shape bound.
    let bounds = Limits { bytes: 100_000, ..Limits::new(0, 0, 8).unwrap() };
    let excessive_nodes = format!("[{}]", vec!["0"; 12].join(","));
    assert!(Saved::decode(&excessive_nodes, bounds).err().unwrap().contains("value limit"));
    assert!(Saved::decode(&format!("\"{}\"", "x".repeat(65)), bounds).err().unwrap().contains("string limit"));
    assert!(Limits::new(usize::MAX, 0, 1024).is_err());
    assert!(Limits::new(0, usize::MAX, 1024).is_err());
}

#[test]
fn malformed_checkpoint_counters_keys_and_digests_cannot_claim_more_than_the_source() {
    let good = saved(3);
    let bounds = limits(&good);
    for change in 0..9 {
        let mut candidate = saved(3);
        match change {
            0 => candidate.checkpoint.next_row += 1,
            1 => candidate.checkpoint.committed_chunks += 1,
            2 => candidate.checkpoint.frontier.0 += 1,
            3 => candidate.rows_per_chunk = 0,
            4 => candidate.source_hash = "X".repeat(64),
            5 => candidate.base_marker.clear(),
            6 => { candidate.checkpoint.vertices.remove("v0"); candidate.checkpoint.edges.insert("v1".into(), EId(9)); },
            7 => { candidate.checkpoint.vertices.remove("v0"); candidate.checkpoint.vertices.insert("".into(), VId(9)); },
            _ => { candidate.checkpoint.vertices.remove("v0"); candidate.checkpoint.vertices.insert("x".repeat(1025), VId(9)); },
        }
        assert!(candidate.view().validate(bounds).is_err());
        assert!(Saved::decode(std::str::from_utf8(&legacy(&candidate)).unwrap(), bounds).is_err());
    }
    let bytes = String::from_utf8(legacy(&good)).unwrap();
    for malformed in [
        bytes.replace("\"v0\":\"1\"", "\"v0\":\"01\""),
        bytes.replace("\"v0\":\"1\"", "\"v0\":1"),
        bytes.replace("\"v\":1", "\"v\":1,\"v\":1"),
        bytes.replace("\"next_row\":3", "\"next_row\":18446744073709551616"),
    ] { assert!(Saved::decode(&malformed, bounds).is_err()); }
}

#[test]
fn checkpoint_reads_preserve_raw_bytes_and_enforce_the_exact_bound_before_decoding() {
    let path = location();
    let value = saved(1);
    let bytes = legacy(&value);
    let bounds = Limits { bytes: bytes.len(), ..limits(&value) };
    assert!(read_controlled(&path, bounds, &mut || Ok(())).unwrap().is_none());
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(read_controlled(&path, bounds, &mut || Ok(())).unwrap().unwrap().as_bytes(), bytes);
    assert!(read_controlled(&path, Limits { bytes: bytes.len() - 1, ..bounds }, &mut || Ok(())).is_err());
    std::fs::write(&path, [0xff]).unwrap();
    assert!(read_controlled(&path, bounds, &mut || Ok(())).unwrap_err().to_string().contains("UTF-8"));
    std::fs::write(&path, b"{\"v\":\"unescaped\nnewline\"}").unwrap();
    let raw = read_controlled(&path, bounds, &mut || Ok(())).unwrap().unwrap();
    assert!(Saved::decode(&raw, bounds).is_err(), "do not join lines and repair invalid JSON");
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(bounds.bytes as u64 + 1).unwrap();
    let mut calls = 0;
    assert!(read_controlled(&path, bounds, &mut || { calls += 1; Ok(()) }).is_err());
    assert_eq!(calls, 2, "oversized metadata must refuse before the first read");
}

#[test]
fn changed_length_and_every_read_checkpoint_return_no_partial_document() {
    let path = location();
    let value = saved(2);
    let bytes = legacy(&value);
    let bounds = limits(&value);
    std::fs::write(&path, &bytes).unwrap();
    // Third checkpoint is immediately before the first read, after metadata.
    let mut calls = 0;
    let changed = read_controlled(&path, bounds, &mut || {
        calls += 1;
        if calls == 3 { OpenOptions::new().append(true).open(&path)?.write_all(b" ")?; }
        Ok(())
    });
    assert!(changed.unwrap_err().to_string().contains("length changed"));
    std::fs::write(&path, &bytes).unwrap();
    let mut calls = 0;
    read_controlled(&path, bounds, &mut || { calls += 1; Ok(()) }).unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        assert!(read_controlled(&path, bounds, &mut || {
            seen += 1;
            if seen == stop { Err(io::Error::from(io::ErrorKind::Interrupted)) } else { Ok(()) }
        }).is_err());
        assert_eq!(seen, stop);
    }
}

struct Counting {
    bytes: usize,
    writes: usize,
    largest: usize,
    hash: fgdb_crypto::Hasher,
}
impl Default for Counting {
    fn default() -> Self {
        Self { bytes: 0, writes: 0, largest: 0, hash: fgdb_crypto::Hasher::new() }
    }
}
impl Write for Counting {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes += bytes.len(); self.writes += 1; self.largest = self.largest.max(bytes.len());
        self.hash.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
#[test]
fn large_identity_maps_use_a_fixed_buffer_and_preserve_canonical_bytes() {
    let value = saved(10_000);
    let expected = legacy(&value);
    let mut sink = Counting::default();
    let mut control = || Ok(());
    {
        let mut writer = Buffer::new(&mut sink, &mut control, limits(&value).bytes).unwrap();
        value.view().write_to(&mut writer, limits(&value)).unwrap();
        assert!(writer.buffer.capacity() <= BUFFER_BYTES);
        writer.flush().unwrap();
    }
    assert_eq!(sink.bytes, expected.len());
    assert_eq!(sink.hash.finalize(), fgdb_crypto::hash(&expected));
    assert!(sink.largest <= BUFFER_BYTES);
    assert!(sink.writes > 1);
}

struct PartialFailure { bytes: Vec<u8>, writes: usize }
impl Write for PartialFailure {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        if self.writes == 1 {
            let n = bytes.len().min(3);
            self.bytes.extend_from_slice(&bytes[..n]);
            Ok(n)
        } else { Err(io::Error::other("injected write failure")) }
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
#[test]
fn drop_never_flushes_and_a_partial_write_error_cannot_replay_the_buffer() {
    let mut sink = Vec::new();
    let mut control = || Ok(());
    {
        let mut out = Buffer::new(&mut sink, &mut control, 100).unwrap();
        out.write_all(b"unpublished").unwrap();
    }
    assert!(sink.is_empty());
    let mut sink = PartialFailure { bytes: Vec::new(), writes: 0 };
    {
        let mut out = Buffer::new(&mut sink, &mut control, 100).unwrap();
        out.write_all(b"abcdefgh").unwrap();
        assert!(out.flush().is_err());
        assert!(out.flush().is_err());
        assert!(out.write(b"retry").is_err());
    }
    assert_eq!(sink.writes, 2, "neither retries nor Drop may touch failed staging");
    assert_eq!(sink.bytes, b"abc");
}

#[test]
fn staging_collisions_never_truncate_another_attempt_and_refuse_after_a_fixed_bound() {
    let path = location();
    let collision = path.with_file_name(format!("checkpoint.tmp.{}.0", std::process::id()));
    std::fs::write(&collision, b"retained-other-attempt").unwrap();
    let mut id = 0;
    let (mut file, stage) = create_stage(&path, &mut || { let n = id; id += 1; Ok(n) }, &mut || Ok(())).unwrap();
    assert_ne!(stage, collision);
    file.write_all(b"private stage").unwrap();
    assert_eq!(std::fs::read(&collision).unwrap(), b"retained-other-attempt");
    assert!(!path.exists());
    let mut attempts = 0;
    assert!(create_stage(&path, &mut || { attempts += 1; Ok(0) }, &mut || Ok(())).is_err());
    assert_eq!(attempts, MAX_STAGE_ATTEMPTS);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o077, 0);
    }
}

#[cfg(unix)]
#[test]
fn colliding_symlinks_are_not_followed_or_overwritten_by_checkpoint_staging() {
    let path = location();
    let target = path.with_file_name("unrelated");
    std::fs::write(&target, b"keep me").unwrap();
    let collision = path.with_file_name(format!("checkpoint.tmp.{}.0", std::process::id()));
    std::os::unix::fs::symlink(&target, &collision).unwrap();
    let mut id = 0;
    let (mut file, _) = create_stage(&path, &mut || { let n = id; id += 1; Ok(n) }, &mut || Ok(())).unwrap();
    file.write_all(b"checkpoint").unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
    assert!(std::fs::symlink_metadata(&collision).unwrap().file_type().is_symlink());
}

#[test]
fn every_publication_checkpoint_preserves_the_old_or_complete_new_file_never_a_prefix() {
    let path = location();
    let old = saved(2);
    let new = saved(3);
    let bounds = limits(&new);
    persist_controlled(&path, old.view(), bounds, &mut || Ok(())).unwrap();
    let mut calls = 0;
    persist_controlled(&path, new.view(), bounds, &mut || { calls += 1; Ok(()) }).unwrap();
    assert!(calls > 10);
    for stop in 1..=calls {
        persist_controlled(&path, old.view(), bounds, &mut || Ok(())).unwrap();
        let mut seen = 0;
        let result = persist_controlled(&path, new.view(), bounds, &mut || {
            seen += 1;
            if seen == stop { Err(io::Error::from(io::ErrorKind::Interrupted)) } else { Ok(()) }
        });
        assert!(result.is_err());
        assert_eq!(seen, stop);
        let actual = std::fs::read(&path).unwrap();
        // The last two controls follow rename: either error can leave the new
        // complete file visible, but neither proves its parent directory synced.
        let expected = if stop >= calls - 1 { legacy(&new) } else { legacy(&old) };
        assert_eq!(actual, expected, "stop {stop}/{calls}");
        assert!(Saved::decode(std::str::from_utf8(&actual).unwrap(), bounds).is_ok());
    }
}

#[test]
fn output_limit_failure_cannot_replace_a_preexisting_checkpoint() {
    let path = location();
    let old = saved(2);
    let new = saved(3);
    let bounds = limits(&new);
    persist_controlled(&path, old.view(), bounds, &mut || Ok(())).unwrap();
    let bounds = Limits { bytes: legacy(&new).len() - 1, ..bounds };
    assert!(persist_controlled(&path, new.view(), bounds, &mut || Ok(())).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), legacy(&old));
}
