//! Transport adaptation for the existing native pull executor.
//!
//! Preparation/source admission precede the header. Each complete encoded row
//! is flushed before requesting another. This bounds retained output to one
//! row, not the underlying decoded source generation or a scalar's encoding
//! scratch. No collect, re-execution with larger LIMIT, or eager fallback.
//! An error after a delivered prefix is terminal and emits no success result.

use super::{Failure, Options, emit, execution_failure, human_value, policy, quoted, value_cell};
use asupersync::fs::Vfs;
use fgdb::{Database, PreparedNativeRead};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_types::QueryCx;
use std::io::Write;

pub(super) fn run<V: Vfs + Clone>(
    db: &Database<V>, cx: &QueryCx, options: &Options, robot: bool, out: &mut impl Write,
) -> Result<(), Failure> {
    let prepared = PreparedNativeRead::prepare(&options.text, &options.params, options)
        .map_err(execution_failure)?;
    let (columns, mut cursor) = prepared.stream(db, cx, &options.params, policy())
        .map_err(execution_failure)?;
    // In particular, a temporal stream names its actual retained cut, not the
    // live writer's later frontier. No second database read supplies metadata.
    let seq = cursor.snapshot_seq().0;
    let result = deliver(&columns, seq, &mut cursor, robot, out, || {
        cx.checkpoint().map_err(Failure::query)
    });
    // Both transport failure and success release the source without scanning
    // an unread suffix. Unwinding also drops the native single-owner cursor.
    cursor.close();
    result
}

// The iterator seam is only delivery, not a graph source or alternate executor.
// Production supplies exactly one native cursor with its original cumulative
// allowances; tests can observe demand and inject output failures at that seam.
fn deliver<E: std::error::Error + 'static>(
    columns: &[String],
    seq: u64,
    rows: &mut impl Iterator<Item = Result<GraphValueRow, E>>,
    robot: bool,
    out: &mut impl Write,
    mut checkpoint: impl FnMut() -> Result<(), Failure>,
) -> Result<(), Failure> {
    let mut sent = 0u64;
    let result = (|| {
        checkpoint()?;
        let header = if robot {
            format!(r#"{{"v":1,"event":"columns","stream":true,"seq":{seq},"columns":[{}]}}"#,
                columns.iter().map(|name| quoted(name)).collect::<Vec<_>>().join(","))
        } else {
            format!("{}\nstream at seq {seq}", columns.iter()
                .map(|name| name.chars().flat_map(char::escape_default).collect::<String>())
                .collect::<Vec<_>>().join("\t"))
        };
        emit(out, &header)?;
        out.flush().map_err(Failure::io)?;
        loop {
            // Known cancellation or broken output must not demand another row.
            checkpoint()?;
            let Some(row) = rows.next() else { break; };
            let row = row.map_err(execution_failure)?;
            if row.values().len() != columns.len() {
                return Err(Failure::query("stream row width does not match native columns"));
            }
            let next = sent.checked_add(1)
                .ok_or_else(|| Failure::query("stream delivery counter overflow"))?;
            let mut encoded = Vec::with_capacity(columns.len());
            for value in row.values() {
                checkpoint()?;
                encoded.push(if robot { value_cell(value)? } else { human_value(value)? });
            }
            let line = if robot {
                format!(r#"{{"v":1,"event":"row","cells":[{}]}}"#, encoded.join(","))
            } else { encoded.join("\t") };
            checkpoint()?;
            emit(out, &line)?;
            out.flush().map_err(Failure::io)?;
            // Only fully flushed rows count as transport delivery. The native
            // cursor independently counts rows handed to this adapter.
            sent = next;
        }
        checkpoint()?;
        let summary = if robot {
            format!(r#"{{"v":1,"event":"result","kind":"rows","stream":true,"seq":{seq},"count":{sent}}}"#)
        } else { format!("{sent} row(s) (stream complete at seq {seq})") };
        emit(out, &summary)?;
        out.flush().map_err(Failure::io)
    })();
    result.map_err(|error: Failure| Failure::new(error.code, error.class, format!(
        "stream incomplete after {sent} fully flushed row(s); output may contain a partial final frame: {}",
        error.message,
    )))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod edge_tests;
