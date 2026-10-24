//! Framing only: the existing JSON parser still owns record syntax and values.
//! This scanner holds no payload and is shared by sealing and every replay.

use super::refused;
use std::io;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    First,
    Next,
    Object,
    Separator,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Piece {
    Skip,
    Byte,
    Last,
}

#[derive(Clone)]
pub(super) struct Array {
    state: State,
    closers: [u8; 64],
    depth: usize,
    quoted: bool,
    escaped: bool,
    bytes: usize,
    records: usize,
    record_limit: usize,
    max_records: usize,
}

pub(super) fn whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

impl Array {
    pub(super) fn new(record_limit: usize, max_records: usize) -> Self {
        Self {
            state: State::Open,
            closers: [0; 64],
            depth: 0,
            quoted: false,
            escaped: false,
            bytes: 0,
            records: 0,
            record_limit,
            max_records,
        }
    }

    pub(super) fn records(&self) -> usize {
        self.records
    }

    pub(super) fn finish(&self) -> io::Result<()> {
        if self.state == State::End {
            Ok(())
        } else {
            Err(refused("MalformedArray: incomplete JSON array"))
        }
    }

    /// Admit each byte before a replay reader can append it to a record buffer.
    /// Only objects are rows. Container matching and string escapes establish
    /// their boundaries; parse_row subsequently validates complete JSON syntax.
    pub(super) fn push(&mut self, byte: u8) -> io::Result<Piece> {
        if self.state != State::Object {
            if whitespace(byte) {
                return Ok(Piece::Skip);
            }
            match (self.state, byte) {
                (State::Open, b'[') => self.state = State::First,
                (State::First | State::Separator, b']') => self.state = State::End,
                (State::Separator, b',') => self.state = State::Next,
                (State::First | State::Next, b'{') => {
                    if self.records >= self.max_records {
                        return Err(refused("SourceLimit: source_rows"));
                    }
                    if self.record_limit == 0 {
                        return Err(refused("SourceLimit: record_bytes"));
                    }
                    self.state = State::Object;
                    self.closers[0] = b'}';
                    self.depth = 1;
                    self.bytes = 1;
                    return Ok(Piece::Byte);
                }
                _ => {
                    return Err(refused(
                        "MalformedArray: expected an object, separator or end",
                    ));
                }
            }
            return Ok(Piece::Skip);
        }
        self.bytes = self
            .bytes
            .checked_add(1)
            .filter(|&bytes| bytes <= self.record_limit)
            .ok_or_else(|| refused("SourceLimit: record_bytes"))?;
        if self.quoted {
            // JSON strings cannot contain raw control bytes, even after '\'.
            if byte < 0x20 {
                return Err(refused("MalformedArray: control byte in JSON string"));
            }
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.quoted = false;
            }
        } else {
            match byte {
                b'"' => self.quoted = true,
                b'{' | b'[' => {
                    if self.depth == self.closers.len() {
                        return Err(refused("SourceLimit: JSON nesting"));
                    }
                    self.closers[self.depth] = if byte == b'{' { b'}' } else { b']' };
                    self.depth += 1;
                }
                b'}' | b']' => {
                    if self.closers[self.depth - 1] != byte {
                        return Err(refused("MalformedArray: mismatched JSON container"));
                    }
                    self.depth -= 1;
                    if self.depth == 0 {
                        self.records = self
                            .records
                            .checked_add(1)
                            .ok_or_else(|| refused("SourceLimit: source_rows"))?;
                        self.state = State::Separator;
                        return Ok(Piece::Last);
                    }
                }
                _ => {}
            }
        }
        Ok(Piece::Byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8], record_limit: usize, max_records: usize) -> io::Result<Vec<Vec<u8>>> {
        let mut framer = Array::new(record_limit, max_records);
        let mut rows = Vec::new();
        let mut row = Vec::new();
        for &byte in bytes {
            match framer.push(byte)? {
                Piece::Skip => {}
                Piece::Byte => row.push(byte),
                Piece::Last => {
                    row.push(byte);
                    rows.push(core::mem::take(&mut row));
                }
            }
        }
        framer.finish()?;
        assert!(row.is_empty());
        assert_eq!(framer.records(), rows.len());
        Ok(rows)
    }

    #[test]
    fn quoted_delimiters_nested_values_and_pretty_printing_preserve_exact_records() {
        let records = [
            r#"{"text":"commas, brackets ] } [ { and \"quotes\" and \\","nested":[{"a":true},null]}"#,
            "{\n  \"unicode\": \"λ😀\",\n  \"empty\": {}\n}",
        ];
        let source = format!(" \r\n[\n{} ,\r\n {}\n]\t", records[0], records[1]);
        let got = decode(source.as_bytes(), 1024, 2).unwrap();
        assert_eq!(got, records.map(|row| row.as_bytes().to_vec()));
        for row in got {
            let text = std::str::from_utf8(&row).unwrap();
            fgdb_protocol::json::parse_json(text, usize::MAX, text.len()).unwrap();
        }
    }

    #[test]
    fn outer_grammar_and_every_incomplete_prefix_refuse() {
        for source in [
            "",
            "[",
            "[{}",
            "[{},]",
            "[,{}]",
            "[{}{}]",
            "[{}]{}",
            "[[{}]]",
            "[null]",
            "[1]",
            "[\"text\"]",
            "[{]",
            "[{\"x\":[}]",
            "[{\"x\":\"\n\"}]",
        ] {
            assert!(decode(source.as_bytes(), 1024, 10).is_err(), "{source:?}");
        }
        let source = br#"[{"x":[{"y":"\\\""}]}]"#;
        for end in 0..source.len() {
            assert!(decode(&source[..end], 1024, 10).is_err(), "prefix {end}");
        }
        assert!(decode(source, 1024, 10).is_ok());
        assert!(decode(b" [ ] \n", 0, 0).unwrap().is_empty());
    }

    #[test]
    fn exact_record_and_row_limits_admit_before_buffering_and_nesting_is_bounded() {
        let source = b" [ {} , {} ] ";
        assert_eq!(decode(source, 2, 2).unwrap().len(), 2);
        assert!(
            decode(source, 1, 2)
                .unwrap_err()
                .to_string()
                .contains("record_bytes")
        );
        assert!(
            decode(source, 2, 1)
                .unwrap_err()
                .to_string()
                .contains("source_rows")
        );
        let mut framer = Array::new(1024, 0);
        assert!(framer.push(b'[').is_ok());
        assert!(
            framer
                .push(b'{')
                .unwrap_err()
                .to_string()
                .contains("source_rows")
        );
        assert_eq!(framer.records(), 0);
        let deeply_nested = format!("[{{\"x\":{}0{}}}]", "[".repeat(64), "]".repeat(64));
        assert!(
            decode(deeply_nested.as_bytes(), 1024, 1)
                .unwrap_err()
                .to_string()
                .contains("nesting")
        );
    }
}

#[cfg(test)]
mod reader_tests {
    use super::super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixture(bytes: &[u8]) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fgdb-array-input-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(bytes).unwrap();
        path
    }

    fn open(path: &Path, record_limit: usize, records: usize) -> io::Result<Input> {
        Input::open_controlled(
            path,
            b"bindings",
            MAX_INPUT_BYTES,
            record_limit,
            records,
            &mut || Ok(()),
        )
    }

    fn next(reader: &mut Reader) -> Option<String> {
        let row = reader.next_controlled(&mut || Ok(())).unwrap();
        assert!(reader.cache.capacity() <= BLOCK_BYTES);
        row
    }

    #[test]
    fn array_replay_clones_and_hashes_preserve_objects_across_block_boundaries() {
        for padding in [BLOCK_BYTES - 1, BLOCK_BYTES, BLOCK_BYTES + 1] {
            let first = format!(
                "{{\n\"value\":\"{}λ😀\",\n\"end\":true\n}}",
                "x".repeat(padding)
            );
            let second = r#"{"value":"escaped \" , ] } \\","end":false}"#;
            let source = format!("{}[\n{first},\r\n{second}\n] \n", " ".repeat(padding));
            let path = fixture(source.as_bytes());
            let input = open(&path, MAX_RECORD_BYTES, 2).unwrap();
            assert_eq!(input.records(), 2);
            assert!(input.image.array);
            let mut digest = Hasher::new();
            digest.update(source.as_bytes());
            digest.update(b"bindings");
            assert_eq!(input.source_hash(), &digest.finalize());
            let mut reader = input.reader();
            let mut beginning = reader.clone();
            assert_eq!(next(&mut reader).as_deref(), Some(first.as_str()));
            assert_eq!(reader.line(), 2);
            let mut remaining = reader.clone();
            assert_eq!(next(&mut remaining).as_deref(), Some(second));
            assert_eq!(next(&mut reader).as_deref(), Some(second));
            assert_eq!(next(&mut beginning).as_deref(), Some(first.as_str()));
            assert_eq!(next(&mut beginning).as_deref(), Some(second));
            for reader in [&mut reader, &mut remaining, &mut beginning] {
                assert!(next(reader).is_none());
                assert!(
                    reader
                        .next_controlled(&mut || panic!("terminal"))
                        .unwrap()
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn array_limits_count_logical_objects_not_physical_lines_or_whole_array_bytes() {
        let path = fixture(b"[\n{\n}\n,\n{\n}\n]\n");
        assert_eq!(open(&path, 3, 2).unwrap().records(), 2);
        assert!(
            open(&path, 2, 2)
                .err()
                .unwrap()
                .to_string()
                .contains("record_bytes")
        );
        assert!(
            open(&path, 3, 1)
                .err()
                .unwrap()
                .to_string()
                .contains("source_rows")
        );
        for bytes in [b"[{},]".as_slice(), b"[{}", b"[{}] false", b"[[],{}]"] {
            assert!(open(&fixture(bytes), MAX_RECORD_BYTES, 10).is_err());
        }
    }

    #[test]
    fn array_clone_rechecks_its_seal_and_preserves_cached_source_bytes() {
        let path = fixture(br#"[{"x":1},{"x":2}]"#);
        let input = open(&path, MAX_RECORD_BYTES, 2).unwrap();
        let mut reader = input.reader();
        assert_eq!(next(&mut reader).as_deref(), Some(r#"{"x":1}"#));
        let mut cloned = reader.clone();
        std::fs::write(&path, br#"[{"x":1},{"x":3}]"#).unwrap();
        assert!(
            cloned
                .next_controlled(&mut || Ok(()))
                .unwrap_err()
                .to_string()
                .contains("SourceChanged")
        );
        assert!(
            cloned
                .next_controlled(&mut || panic!("fused"))
                .unwrap()
                .is_none()
        );
        assert_eq!(next(&mut reader).as_deref(), Some(r#"{"x":2}"#));
    }

    #[test]
    fn array_eof_checks_length_and_invalid_utf8_fuses_at_the_object_ordinal() {
        let path = fixture(b"[{\"x\":\"\xff\"}]");
        let input = open(&path, MAX_RECORD_BYTES, 1).unwrap();
        let mut reader = input.reader();
        assert!(
            reader
                .next_controlled(&mut || Ok(()))
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
        assert_eq!(reader.line(), 1);
        assert!(
            reader
                .next_controlled(&mut || panic!("fused"))
                .unwrap()
                .is_none()
        );
        let path = fixture(b"[{}]");
        let input = open(&path, MAX_RECORD_BYTES, 1).unwrap();
        let mut reader = input.reader();
        assert_eq!(next(&mut reader).as_deref(), Some("{}"));
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b" ")
            .unwrap();
        assert!(
            reader
                .next_controlled(&mut || Ok(()))
                .unwrap_err()
                .to_string()
                .contains("length changed")
        );
    }

    #[test]
    fn every_array_read_checkpoint_can_cancel_without_delivering_an_object() {
        let source = format!("[{{\"x\":\"{}\"}}]", "x".repeat(BLOCK_BYTES + 7));
        let path = fixture(source.as_bytes());
        let input = open(&path, MAX_RECORD_BYTES, 1).unwrap();
        let mut calls = 0;
        input
            .reader()
            .next_controlled(&mut || {
                calls += 1;
                Ok(())
            })
            .unwrap();
        for stop in 1..=calls {
            let mut seen = 0;
            let mut reader = input.reader();
            assert!(
                reader
                    .next_controlled(&mut || {
                        seen += 1;
                        if seen == stop {
                            Err(io::ErrorKind::Interrupted.into())
                        } else {
                            Ok(())
                        }
                    })
                    .is_err()
            );
            assert_eq!(seen, stop);
            assert!(
                reader
                    .next_controlled(&mut || panic!("fused"))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn a_large_single_line_array_keeps_one_record_and_one_block_cache() {
        let path = fixture(b"[");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let row = format!("{{\"x\":\"{}\"}}", "x".repeat(8000));
        for index in 0..1000 {
            if index != 0 {
                file.write_all(b",").unwrap();
            }
            file.write_all(row.as_bytes()).unwrap();
        }
        file.write_all(b"]").unwrap();
        drop(file);
        let input = open(&path, row.len(), 1000).unwrap();
        assert_eq!(input.records(), 1000);
        let mut reader = input.reader();
        for _ in 0..1000 {
            assert_eq!(next(&mut reader).as_deref(), Some(row.as_str()));
        }
        assert!(next(&mut reader).is_none());
        assert_eq!(
            input.image.blocks.len(),
            (input.image.len as usize).div_ceil(BLOCK_BYTES)
        );
    }
}
