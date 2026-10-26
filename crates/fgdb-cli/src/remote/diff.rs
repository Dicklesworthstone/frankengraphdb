//! Finite historical NET differences over FGP. Only the exact END confirms
//! completion; each signed change is flushed before another frame is read.

use super::{Client, ClientError, Failure, WireValue, cell, emit, failure, human};
use fgdb_protocol::body::{DiffMode, DiffResultEnd};
use std::io::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Options {
    pub before: u64,
    pub after: u64,
    pub mode: DiffMode,
}

pub(super) fn parse_cut(raw: &str, flag: &str) -> Result<u64, Failure> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Failure::usage(format!("{flag} needs a decimal revision")));
    }
    raw.parse()
        .map_err(|_| Failure::usage(format!("{flag} is out of range")))
}

pub(super) async fn execute(
    cx: &asupersync::Cx,
    client: &mut Client,
    options: Options,
    statement: &str,
    parameters: Vec<(String, WireValue)>,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let sink = std::sync::Mutex::new(Output {
        out,
        robot,
        options,
        width: None,
        sent: 0,
        inserted: 0,
        retracted: 0,
        failed: None,
    });
    let lock = || {
        sink.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    let result = client
        .execute_diff_streaming(
            cx,
            options.mode,
            options.before,
            options.after,
            statement,
            parameters,
            |columns| lock().columns(cx, columns),
            |weight, row| lock().entry(cx, weight, &row),
        )
        .await;
    sink.into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .finish(result)
}

struct Output<'a, W> {
    out: &'a mut W,
    robot: bool,
    options: Options,
    width: Option<usize>,
    sent: u64,
    inserted: u128,
    retracted: u128,
    failed: Option<Failure>,
}

impl<W: Write> Output<'_, W> {
    fn capture(&mut self, result: Result<(), Failure>) -> Result<(), ClientError> {
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failed.get_or_insert(error);
                // The actual CLI failure is retained above; this marker asks
                // the client to cancel and drain before another request.
                Err(ClientError::Io(std::io::ErrorKind::BrokenPipe))
            }
        }
    }

    fn columns(&mut self, cx: &asupersync::Cx, columns: &[String]) -> Result<(), ClientError> {
        let result = (|| {
            cx.checkpoint().map_err(Failure::query)?;
            let before = self.options.before;
            let after = self.options.after;
            let names = columns
                .iter()
                .map(|name| {
                    if self.robot {
                        fgdb_protocol::json::quote(name)
                    } else {
                        name.chars().flat_map(char::escape_default).collect()
                    }
                })
                .collect::<Vec<_>>();
            let line = if self.robot {
                format!(
                    r#"{{"v":1,"event":"diff_columns","before":"{before}","after":"{after}","semantics":"after_minus_before_bag","columns":[{}]}}"#,
                    names.join(",")
                )
            } else {
                format!(
                    "diff {before} -> {after} (net occurrence changes)\nweight\t{}",
                    names.join("\t")
                )
            };
            emit(self.out, &line)?;
            self.out.flush().map_err(Failure::io)?;
            self.width = Some(columns.len());
            Ok(())
        })();
        self.capture(result)
    }

    fn entry(
        &mut self,
        cx: &asupersync::Cx,
        weight: i128,
        row: &[WireValue],
    ) -> Result<(), ClientError> {
        let result = (|| {
            cx.checkpoint().map_err(Failure::query)?;
            if weight == 0 || self.width != Some(row.len()) {
                return Err(Failure::io(
                    "remote diff row does not match its signed columns contract",
                ));
            }
            let next = self
                .sent
                .checked_add(1)
                .ok_or_else(|| Failure::query("diff delivery count overflow"))?;
            let total = if weight > 0 {
                self.inserted
            } else {
                self.retracted
            };
            let total = total
                .checked_add(weight.unsigned_abs())
                .ok_or_else(|| Failure::query("diff delivery occurrence total overflow"))?;
            let mut cells = Vec::with_capacity(row.len());
            for value in row {
                cx.checkpoint().map_err(Failure::query)?;
                cells.push(if self.robot {
                    cell(value)
                } else {
                    human(value)
                });
            }
            let line = if self.robot {
                format!(
                    r#"{{"v":1,"event":"change","weight":"{weight}","cells":[{}]}}"#,
                    cells.join(",")
                )
            } else {
                format!("{weight:+}\t{}", cells.join("\t"))
            };
            emit(self.out, &line)?;
            self.out.flush().map_err(Failure::io)?;
            self.sent = next;
            if weight > 0 {
                self.inserted = total;
            } else {
                self.retracted = total;
            }
            Ok(())
        })();
        self.capture(result)
    }

    fn finish(self, result: Result<DiffResultEnd, ClientError>) -> Result<(), Failure> {
        let incomplete = |error: Failure| Failure {
            message: format!(
                "diff incomplete after {} fully flushed change(s); output may contain a partial final frame: {}",
                self.sent, error.message
            ),
            ..error
        };
        if let Some(error) = self.failed {
            return Err(incomplete(error));
        }
        let result = (|| {
            let end = result.map_err(failure)?;
            if end.before != self.options.before
                || end.after != self.options.after
                || end.rows != self.sent
            {
                return Err(Failure::io(
                    "remote diff completion does not match delivered changes",
                ));
            }
            let before = end.before;
            let after = end.after;
            let sent = self.sent;
            let inserted = self.inserted;
            let retracted = self.retracted;
            let line = if self.robot {
                format!(
                    r#"{{"v":1,"event":"result","kind":"diff","before":"{before}","after":"{after}","changed_rows":"{sent}","inserted":"{inserted}","retracted":"{retracted}"}}"#
                )
            } else {
                format!(
                    "{sent} changed tuple(s): +{inserted} / -{retracted} occurrence(s) (diff complete {before} -> {after})"
                )
            };
            emit(self.out, &line)?;
            self.out.flush().map_err(Failure::io)
        })();
        result.map_err(incomplete)
    }
}
