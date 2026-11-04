//! Transport-independent validation of a complete consistent read batch.
//! Validation continues after callback cancellation: an interrupted or malformed
//! prefix can never become a successful answer.

use crate::body::{ReadBatchResultChunk, ReadBatchResultEnd};

#[derive(Clone, Copy)]
struct Active {
    seq: u64,
    width: usize,
    rows: u64,
}

pub(crate) struct ReadBatchProgress {
    expected: u32,
    next: u32,
    active: Option<Active>,
    rows: u64,
}

impl ReadBatchProgress {
    pub(crate) fn new(expected: u32) -> Self {
        Self {
            expected,
            next: 0,
            active: None,
            rows: 0,
        }
    }

    /// Validate the entire fragment before callbacks. A completed result
    /// returns its row count, including earlier fragments. Rejected input
    /// changes neither the accepted schema nor the accepted progress.
    pub(crate) fn observe(
        &mut self,
        part: &ReadBatchResultChunk,
    ) -> Result<Option<u64>, &'static str> {
        if part.index != self.next || self.next >= self.expected {
            return Err("read batch result index out of order");
        }
        let (width, preceding) = match (&part.chunk.columns, self.active) {
            (Some(names), None) => (names.len(), 0),
            (None, Some(active)) if active.seq == part.seq => (active.width, active.rows),
            (None, Some(_)) => return Err("read batch result changed selected sequence"),
            _ => return Err("read batch columns out of order"),
        };
        if part.chunk.rows.iter().any(|row| row.len() != width) {
            return Err("read batch row width differs from columns");
        }
        if part.chunk.columns.is_none() && part.chunk.rows.is_empty() && !part.last {
            return Err("empty read batch continuation");
        }
        let count =
            u64::try_from(part.chunk.rows.len()).map_err(|_| "read batch row count overflow")?;
        let rows = preceding
            .checked_add(count)
            .ok_or("read batch row count overflow")?;
        let total = self
            .rows
            .checked_add(count)
            .ok_or("read batch row count overflow")?;
        self.rows = total;
        if part.last {
            // The requested count bounds next before this increment.
            self.next += 1;
            self.active = None;
            Ok(Some(rows))
        } else {
            self.active = Some(Active {
                seq: part.seq,
                width,
                rows,
            });
            Ok(None)
        }
    }

    pub(crate) fn finish(&self, end: &ReadBatchResultEnd) -> Result<(), &'static str> {
        if self.active.is_some() || self.next != self.expected {
            return Err("read batch END before every result completed");
        }
        if end.results != self.expected || end.rows != self.rows {
            return Err("read batch END counts differ from delivery");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::{Body, BodyError, ResultChunk, WireValue};

    fn part(index: u32, seq: u64, first: bool, last: bool, values: &[i64]) -> ReadBatchResultChunk {
        ReadBatchResultChunk {
            index,
            seq,
            last,
            chunk: ResultChunk {
                columns: first.then(|| vec!["value".into()]),
                rows: values
                    .iter()
                    .map(|value| vec![WireValue::Int(*value)])
                    .collect(),
            },
        }
    }

    #[test]
    fn batch_bodies_have_exact_bytes_and_reject_every_truncated_or_noncanonical_form() {
        let chunk = ReadBatchResultChunk {
            index: 7,
            seq: 9,
            last: true,
            chunk: ResultChunk {
                columns: Some(vec!["x".into()]),
                rows: vec![],
            },
        };
        let bytes = vec![
            0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 9, 1, 1, 0, 0, 0, 1, 0, 0, 0, 1, b'x', 0, 0, 0, 0,
        ];
        assert_eq!(chunk.encode().unwrap(), bytes);
        assert_eq!(ReadBatchResultChunk::decode(&bytes).unwrap(), chunk);
        for end in 0..bytes.len() {
            assert!(
                ReadBatchResultChunk::decode(&bytes[..end]).is_err(),
                "prefix {end}"
            );
        }
        let mut invalid = bytes.clone();
        invalid.push(0);
        assert_eq!(
            ReadBatchResultChunk::decode(&invalid),
            Err(BodyError::TrailingBytes)
        );
        for flag in [12, 13] {
            let mut invalid = bytes.clone();
            invalid[flag] = 2;
            assert!(ReadBatchResultChunk::decode(&invalid).is_err());
        }
        let mut invalid = bytes;
        invalid[..4].copy_from_slice(&1024_u32.to_be_bytes());
        assert_eq!(
            ReadBatchResultChunk::decode(&invalid),
            Err(BodyError::TooLarge)
        );

        let nested = ReadBatchResultChunk {
            index: 0,
            seq: u64::MAX,
            last: false,
            chunk: ResultChunk {
                columns: Some(vec!["payload".into()]),
                rows: vec![vec![WireValue::Map(vec![(
                    "items".into(),
                    WireValue::List(vec![
                        WireValue::Vertex(u128::MAX),
                        WireValue::Int(-7),
                        WireValue::Null,
                    ]),
                )])]],
            },
        };
        let bytes = nested.encode().unwrap();
        assert_eq!(ReadBatchResultChunk::decode(&bytes).unwrap(), nested);
        for end in 0..bytes.len() {
            assert!(ReadBatchResultChunk::decode(&bytes[..end]).is_err());
        }

        let end = ReadBatchResultEnd {
            results: 8,
            rows: u64::MAX,
        };
        let bytes = vec![0, 0, 0, 8, 255, 255, 255, 255, 255, 255, 255, 255];
        assert_eq!(end.encode().unwrap(), bytes);
        assert_eq!(ReadBatchResultEnd::decode(&bytes).unwrap(), end);
        for cut in 0..bytes.len() {
            assert!(ReadBatchResultEnd::decode(&bytes[..cut]).is_err());
        }
        for count in [0_u32, 1025] {
            assert!(
                ReadBatchResultEnd {
                    results: count,
                    rows: 0,
                }
                .encode()
                .is_err()
            );
            let mut invalid = bytes.clone();
            invalid[..4].copy_from_slice(&count.to_be_bytes());
            assert!(ReadBatchResultEnd::decode(&invalid).is_err());
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(
            ReadBatchResultEnd::decode(&trailing),
            Err(BodyError::TrailingBytes)
        );
    }

    #[test]
    fn fragmented_and_empty_results_keep_individual_cuts_until_one_complete_terminal() {
        let mut progress = ReadBatchProgress::new(3);
        assert_eq!(
            progress.observe(&part(0, 17, true, false, &[8, 9])),
            Ok(None)
        );
        assert_eq!(
            progress.observe(&part(0, 17, false, true, &[10])),
            Ok(Some(3))
        );
        assert_eq!(progress.observe(&part(1, 3, true, true, &[])), Ok(Some(0)));
        assert!(
            progress
                .finish(&ReadBatchResultEnd {
                    results: 2,
                    rows: 3,
                })
                .is_err()
        );
        assert_eq!(
            progress.observe(&part(2, u64::MAX, true, true, &[11])),
            Ok(Some(1))
        );
        assert_eq!(
            progress.finish(&ReadBatchResultEnd {
                results: 3,
                rows: 4,
            }),
            Ok(())
        );
        assert!(progress.observe(&part(3, 17, true, true, &[])).is_err());
        for end in [
            ReadBatchResultEnd {
                results: 2,
                rows: 4,
            },
            ReadBatchResultEnd {
                results: 4,
                rows: 4,
            },
            ReadBatchResultEnd {
                results: 3,
                rows: 3,
            },
            ReadBatchResultEnd {
                results: 3,
                rows: 5,
            },
        ] {
            assert!(progress.finish(&end).is_err(), "{end:?}");
        }
    }

    #[test]
    fn malformed_fragments_cannot_advance_result_order_schema_or_sequence() {
        let mut progress = ReadBatchProgress::new(2);
        assert!(progress.observe(&part(1, 7, true, true, &[])).is_err());
        assert!(progress.observe(&part(0, 7, false, true, &[])).is_err());
        assert_eq!(progress.observe(&part(0, 7, true, false, &[1])), Ok(None));
        assert!(
            progress
                .finish(&ReadBatchResultEnd {
                    results: 2,
                    rows: 1,
                })
                .is_err()
        );
        for invalid in [
            part(1, 7, true, true, &[]),
            part(0, 7, true, true, &[2]),
            part(0, 8, false, true, &[2]),
            part(0, 7, false, false, &[]),
            ReadBatchResultChunk {
                chunk: ResultChunk {
                    columns: None,
                    rows: vec![vec![]],
                },
                ..part(0, 7, false, true, &[])
            },
        ] {
            assert!(progress.observe(&invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(
            progress.observe(&part(0, 7, false, true, &[2])),
            Ok(Some(2))
        );
        assert!(progress.observe(&part(0, 7, true, true, &[])).is_err());
        assert_eq!(progress.observe(&part(1, 9, true, true, &[3])), Ok(Some(1)));
        assert_eq!(
            progress.finish(&ReadBatchResultEnd {
                results: 2,
                rows: 3,
            }),
            Ok(())
        );
    }
}
