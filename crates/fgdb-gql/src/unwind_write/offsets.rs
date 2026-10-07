//! Provenance for short imported aliases expanded into native parameter holes.
//! Coordinates, not text/values, are the only metadata retained here.

use super::GraphUnwindWriteText;
use crate::{GraphMutationProgramTemplateError as M, GraphWriteProgramTemplateError as W};
use crate::{GraphWriteScriptError, GraphWriteScriptErrorKind};
use core::ops::Range;

#[derive(Clone)]
pub(crate) struct UnwindSourceOffset {
    pub(crate) generated: Range<usize>,
    pub(crate) original: Range<usize>,
}

impl GraphUnwindWriteText {
    /// Original offset for an unchanged byte/boundary. Interior generated
    /// parameter bytes identify the beginning of the original alias token.
    /// The parser seals sorted, nonoverlapping expansions only; no contraction
    /// or untrusted caller-supplied mapping is possible.
    pub(crate) fn original_offset(&self, generated: usize) -> usize {
        let before = self.source_offsets.partition_point(|edit| edit.generated.start <= generated);
        let Some(edit) = before.checked_sub(1).map(|index| &self.source_offsets[index]) else {
            return generated;
        };
        if generated < edit.generated.end {
            edit.original.start
        } else {
            generated - (edit.generated.end - edit.original.end)
        }
    }

    /// Preserve both the script-global coordinate and the original native
    /// statement-local cause. The existing script wrapper has already added
    /// its span start to that local offset. Arithmetic and schema error kinds
    /// (and every supplied value) remain untouched.
    pub(crate) fn restore_script_offsets(&self, error: &mut GraphWriteScriptError) {
        let global = error.offset;
        let local = match &mut error.kind {
            GraphWriteScriptErrorKind::Program(source) => match source {
                W::InsertBind { source, .. } => Some(&mut source.offset),
                W::VertexMergeBind { source, .. } => Some(&mut source.offset),
                W::VertexUpsertBind { source, .. } => Some(&mut source.offset),
                W::EdgeMergeBind { source, .. } => Some(&mut source.offset),
                W::EdgeUpsertBind { source, .. } => Some(&mut source.offset),
                W::DeleteBind { source, .. } => Some(&mut source.offset),
                W::Program(M::Bind { source, .. }) => Some(&mut source.offset),
                W::Program(M::ConflictingParameterTypes { .. }
                    | M::Definition(_) | M::UnexpectedArguments) => None,
            },
            _ => None,
        };
        if let Some(local) = local {
            // Every native source above supplies this relation. Keeping the
            // guard makes diagnostic restoration total even on a bad internal
            // coordinate, without panicking while reporting the original error.
            if let Some(base) = global.checked_sub(*local) {
                *local = self.original_offset(global) - self.original_offset(base);
            }
        }
        error.offset = self.original_offset(global);
    }
}
