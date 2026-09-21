//! Read-only revision comparisons through the public native diff engine.
//!
//! Complete endpoint execution precedes transport. Keep the engine's canonical
//! compressed changes and exact cells; never replay writes, expand weights or
//! compare text renderings. Delivery retains one encoded row at a time, not a
//! second result table. Source/result state remains governed in-memory state.

use super::{Failure, Options, cell, emit, execution_failure, human_value, policy, quoted};
use asupersync::fs::Vfs;
use fgdb::{Database, QueryValue};
use fgdb_delta_types::ZWeight;
use fgdb_gql::GqlQueryPolicy;
use fgdb_gql::result_diff::GraphResultDiff;
use fgdb_types::{CommitSeq, QueryCx};
use std::io::Write;

#[derive(Default)]
pub(super) struct DiffOptions {
    before: Option<u64>,
    after: Option<u64>,
    records: Option<u64>,
    rows: Option<u64>,
    work: Option<u64>,
    scratch: Option<u64>,
}
impl DiffOptions {
    pub(super) fn set(&mut self, flag: &str, raw: &str) -> Result<(), Failure> {
        let target = match flag {
            "--before" => &mut self.before,
            "--after" => &mut self.after,
            "--max-snapshot-records" => &mut self.records,
            "--max-result-rows" => &mut self.rows,
            "--max-work-units" => &mut self.work,
            "--max-scratch-entries" => &mut self.scratch,
            _ => return Err(Failure::usage("unknown diff flag")),
        };
        if target.is_some() {
            return Err(Failure::usage(format!("{flag} must be supplied only once")));
        }
        // Reject signs, whitespace, fractions and overflow, without echoing
        // parameter/query payloads. Never parse through float or a platform usize.
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Failure::usage(format!("{flag} requires decimal u64")));
        }
        *target = Some(raw.parse().map_err(|_| Failure::usage(format!("{flag} exceeds u64")))?);
        Ok(())
    }

    pub(super) fn validate(&self) -> Result<(), Failure> {
        self.endpoints().map(|_| ())
    }

    fn endpoints(&self) -> Result<(CommitSeq, CommitSeq), Failure> {
        Ok((
            CommitSeq(self.before.ok_or_else(|| Failure::usage("diff requires --before"))?),
            CommitSeq(self.after.ok_or_else(|| Failure::usage("diff requires --after"))?),
        ))
    }

    fn policy(&self) -> GqlQueryPolicy {
        let defaults = policy();
        GqlQueryPolicy::new(
            self.records.unwrap_or(defaults.rows.max_snapshot_records().unwrap_or(u64::MAX)),
            self.rows.unwrap_or(defaults.rows.max_result_rows().unwrap_or(u64::MAX)),
            self.work.unwrap_or(defaults.evaluator.max_work_units),
            self.scratch.unwrap_or(defaults.evaluator.max_scratch_entries),
        )
    }
}

pub(super) fn run<V: Vfs + Clone>(
    db: &Database<V>, cx: &QueryCx, options: &Options, robot: bool, out: &mut impl Write,
) -> Result<(), Failure> {
    let (before, after) = options.diff.endpoints()?;
    // This public API owns both revision fences, one frozen binding, full query
    // semantics and cumulative source/work/scratch admission. No write authority
    // or intermediate event stream is acquired by this command.
    let result = db.query_diff(cx, &options.text, &options.params, options, before, after,
        options.diff.policy()).map_err(execution_failure)?;
    render(&result, robot, out, &mut || cx.checkpoint().map_err(Failure::query))
}

fn signed(weight: &ZWeight) -> Result<i128, Failure> {
    // GraphResultDiff subtracts two Vec-backed occurrence bags, so each weight
    // fits i128. Refuse a future incompatible carrier, never truncate/saturate.
    weight.to_i128().filter(|value| *value != 0)
        .ok_or_else(|| Failure::query("diff has an invalid or unsupported occurrence weight"))
}

fn render(
    result: &GraphResultDiff, robot: bool, out: &mut impl Write,
    checkpoint: &mut impl FnMut() -> Result<(), Failure>,
) -> Result<(), Failure> {
    let (before, after) = (result.before().0, result.after().0);
    let mut inserted = 0_u128;
    let mut retracted = 0_u128;
    // Validate all weight/width assumptions and totals before any diff header.
    // A zero net sum is not an empty change set. Report both signs separately.
    for (row, weight) in result.changes().iter() {
        checkpoint()?;
        if row.len() != result.columns().len() {
            return Err(Failure::query("diff row width does not match its native columns"));
        }
        let value = signed(weight)?;
        let total = if value > 0 { &mut inserted } else { &mut retracted };
        *total = total.checked_add(value.unsigned_abs())
            .ok_or_else(|| Failure::query("diff delivery occurrence total overflow"))?;
    }
    let mut sent = 0_u64;
    let delivery = (|| {
        checkpoint()?;
        let mut names = Vec::new();
        for name in result.columns() {
            checkpoint()?;
            names.push(if robot { quoted(name) }
                else { name.chars().flat_map(char::escape_default).collect() });
        }
        let header = if robot {
            format!(r#"{{"v":1,"event":"diff_columns","before":"{before}","after":"{after}","semantics":"after_minus_before_bag","columns":[{}]}}"#, names.join(","))
        } else {
            format!("diff {before} -> {after} (net occurrence changes)\nweight\t{}", names.join("\t"))
        };
        checkpoint()?;
        emit(out, &header)?;
        out.flush().map_err(Failure::io)?;
        for (row, weight) in result.changes().iter() {
            checkpoint()?;
            let weight = signed(weight)?;
            let mut cells = Vec::new();
            for value in row.iter() {
                checkpoint()?;
                cells.push(if robot { cell(value)? } else {
                    match value {
                        QueryValue::Value(value) => human_value(value)?,
                        QueryValue::Count(value) => value.to_string(),
                        QueryValue::Integer(value) => value.to_string(),
                        QueryValue::Average(value) => value.to_string(),
                    }
                });
            }
            let line = if robot {
                format!(r#"{{"v":1,"event":"change","weight":"{weight}","cells":[{}]}}"#, cells.join(","))
            } else {
                format!("{weight:+}\t{}", cells.join("\t"))
            };
            let next = sent.checked_add(1).ok_or_else(|| Failure::query("diff delivery count overflow"))?;
            checkpoint()?;
            emit(out, &line)?;
            out.flush().map_err(Failure::io)?;
            sent = next; // Count only a fully flushed change record.
        }
        checkpoint()?;
        let rows = result.row_stats();
        let stats = result.evaluator_stats();
        let summary = if robot {
            format!(r#"{{"v":1,"event":"result","kind":"diff","before":"{before}","after":"{after}","changed_rows":"{sent}","inserted":"{inserted}","retracted":"{retracted}","snapshot_records":"{}","work_units":"{}","scratch_entries":"{}"}}"#,
                rows.snapshot_records, stats.work_units, stats.scratch_entries)
        } else {
            format!("{sent} changed tuple(s): +{inserted} / -{retracted} occurrence(s) (diff complete {before} -> {after})")
        };
        emit(out, &summary)?;
        out.flush().map_err(Failure::io)
    })();
    delivery.map_err(|error: Failure| Failure::new(error.code, error.class, format!(
        "diff incomplete after {sent} fully flushed change(s); output may contain a partial final frame: {}",
        error.message,
    )))
}

#[cfg(test)]
mod tests;
