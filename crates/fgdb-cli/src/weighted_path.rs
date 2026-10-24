//! Explicit typed endpoint anchoring for the native weighted-path text profile.
//! The local owner supplies complete vertex identities; numeric query arguments
//! never stand in for graph identity. Preparation finishes before graph open.
//! Execution and ordering are entirely the existing governed pathfinder's.

use super::{Failure, Options, path_cell, render_rows_iter};
use fgdb::EmbeddedReadView;
use fgdb_gql::{
    BoundGraphCheapestPathQuery, GqlQueryPolicy, GraphCostPath, PreparedGraphCheapestPathText,
};
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::io::Write;

#[derive(Default)]
pub(super) struct PathFlags {
    source: Option<VId>,
    target: Option<VId>,
    as_of: Option<u64>,
}

impl PathFlags {
    pub(super) const FLAGS: [&str; 2] = ["--path-source", "--path-target"];

    pub(super) fn enabled(&self) -> bool {
        self.source.is_some() || self.target.is_some()
    }

    pub(super) fn set(&mut self, flag: &str, raw: &str) -> Result<(), Failure> {
        if flag == "--as-of" {
            return super::set_decimal(&mut self.as_of, flag, raw);
        }
        let slot = match flag {
            "--path-source" => &mut self.source,
            "--path-target" => &mut self.target,
            _ => return Err(Failure::usage("unknown weighted-path flag")),
        };
        if slot.is_some() {
            return Err(Failure::usage(format!("{flag} must be supplied only once")));
        }
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Failure::usage(format!(
                "{flag} requires a decimal vertex identity"
            )));
        }
        let id = raw
            .parse::<u128>()
            .map_err(|_| Failure::usage(format!("{flag} exceeds the vertex identity range")))?;
        *slot = Some(VId(id));
        Ok(())
    }

    pub(super) fn validate(&self) -> Result<(), Failure> {
        match (self.source, self.target) {
            (Some(_), Some(_)) | (None, None) => Ok(()),
            _ => Err(Failure::usage(
                "--path-source and --path-target must be supplied together",
            )),
        }
    }
}

/// No graph, resolver, argument map or unparsed statement survives preparation.
pub(super) struct Prepared {
    request: BoundGraphCheapestPathQuery,
    as_of: Option<CommitSeq>,
    policy: GqlQueryPolicy,
}

pub(super) fn prepare(options: &Options) -> Result<Prepared, Failure> {
    let (Some(source), Some(target)) = (options.weighted_path.source, options.weighted_path.target)
    else {
        return Err(Failure::usage(
            "--path-source and --path-target are required for weighted paths",
        ));
    };
    // Match the ordinary query host's one optional final terminator. Interior
    // or repeated semicolons still reach the parser and refuse.
    let text = options.text.trim_end();
    let text = text.strip_suffix(';').unwrap_or(text);
    let template =
        PreparedGraphCheapestPathText::prepare(text, options).map_err(Failure::statement)?;
    let request = template
        .bind(source, target, &options.params)
        .map_err(Failure::statement)?;
    Ok(Prepared {
        request,
        as_of: options.weighted_path.as_of.map(CommitSeq),
        policy: options.budget.policy(),
    })
}

/// Topology and weights use one exact cut of this immutable read view. Finish
/// native admission and path selection before emitting any result bytes.
pub(super) fn run(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    prepared: &Prepared,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let seq = prepared.as_of.unwrap_or_else(|| view.frontier());
    let execution = view
        .execute_graph_cheapest_path_text_governed_at(cx, &prepared.request, seq, prepared.policy)
        .map_err(super::execution_failure)?;
    let name = prepared.request.column_name();
    let columns = [name.to_owned(), format!("{name}_cost")];
    // Only the currently rendered row is encoded. Path identities are borrowed
    // from the native result, with no second path collection or numeric cast.
    render_rows_iter(
        &columns,
        execution.value.iter().map(|row| cells(row, robot)),
        seq.0,
        "rows",
        robot,
        out,
    )
}

fn cells(row: &GraphCostPath, robot: bool) -> [String; 2] {
    let path = path_cell(row.path());
    let cost = row.cost();
    if robot {
        [
            format!(r#"{{"type":"path","value":{path}}}"#),
            format!(r#"{{"type":"wideint","value":"{cost}"}}"#),
        ]
    } else {
        [format!("path({path})"), cost.to_string()]
    }
}

#[cfg(test)]
mod tests;
