//! Source-derived census of logical reference targets in the design plan.
//!
//! Appendix A's reservation table is permanent allocation state.  It must not
//! also be the authority for deciding which targets need reservations.  This
//! module derives that set from the plan bytes, retaining every qualifying
//! occurrence so a duplicated, rewrapped, moved, or retargeted reference
//! changes the pinned transcript.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::hash::sha256_hex;

/// One concrete target alternative inside a reference wrapper.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct ReferenceOccurrence {
    pub family: String,
    pub wrapper: String,
    pub target_expression: String,
    pub line: usize,
    pub column: usize,
}

/// All source occurrences grouped by their normalized target family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceTarget {
    pub family: String,
    pub wrappers: Vec<String>,
    pub target_expressions: Vec<String>,
    pub occurrences: Vec<ReferenceOccurrence>,
}

/// Deterministic full-plan reference census and its release transcripts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceCensus {
    pub targets: Vec<ReferenceTarget>,
    pub occurrences: Vec<ReferenceOccurrence>,
    pub target_count: usize,
    pub target_ids_sha256: String,
    pub occurrence_count: usize,
    pub occurrence_transcript_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceCensusError {
    pub code: &'static str,
    pub line: usize,
    pub column: usize,
}

/// Derive every logical target of `StrongRef`,
/// `CertifiedRemoteStrongRef`, and a concrete `Conditional*Ref` wrapper.
///
/// The scanner is intentionally input-driven: it has no plan line constants,
/// catalog rows, or released target allowlist.  Top-level union shorthand is
/// expanded, nested generic arguments remain part of the exact normalized
/// target expression, and schema metavariables are not concrete targets.
pub fn census_plan_references(source: &[u8]) -> Result<ReferenceCensus, ReferenceCensusError> {
    let source_map = SourceMap::new(source);
    let text = std::str::from_utf8(source).map_err(|error| {
        let offset = error.valid_up_to();
        let (line, column) = source_map.utf8_prefix_line_column(source, offset);
        ReferenceCensusError {
            code: "reference_source_not_utf8",
            line,
            column,
        }
    })?;
    if let Some(offset) = source.iter().position(|byte| *byte == b'\r') {
        let (line, column) = source_map.line_column(text, offset);
        return Err(ReferenceCensusError {
            code: "reference_source_not_lf",
            line,
            column,
        });
    }

    let bytes = text.as_bytes();
    let mut occurrences = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if !identifier_start(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        let identifier_start = cursor;
        cursor += 1;
        while cursor < bytes.len() && identifier_continue(bytes[cursor]) {
            cursor += 1;
        }
        let wrapper = &text[identifier_start..cursor];
        if !qualifying_wrapper(wrapper) {
            continue;
        }
        let open = skip_unicode_whitespace(text, cursor);
        if bytes.get(open) != Some(&b'<') {
            continue;
        }
        let close = matching_angle(bytes, open).ok_or_else(|| {
            let (line, column) = source_map.line_column(text, open);
            ReferenceCensusError {
                code: "reference_wrapper_unclosed",
                line,
                column,
            }
        })?;
        for (alternative_start, alternative_end) in
            top_level_alternatives(text, open + 1, close, &source_map)?
        {
            let trimmed_start = first_non_whitespace(text, alternative_start, alternative_end);
            let trimmed_end = last_non_whitespace(text, trimmed_start, alternative_end);
            if trimmed_start == trimmed_end {
                let (line, column) = source_map.line_column(text, alternative_start);
                return Err(ReferenceCensusError {
                    code: "reference_target_empty",
                    line,
                    column,
                });
            }
            let raw = &text[trimmed_start..trimmed_end];
            let target_expression = normalize_whitespace(raw);
            let Some(family) = leading_family(&target_expression) else {
                let (line, column) = source_map.line_column(text, trimmed_start);
                return Err(ReferenceCensusError {
                    code: "reference_target_unparsed",
                    line,
                    column,
                });
            };
            if schema_metavariable(family) {
                continue;
            }
            let (line, column) = source_map.line_column(text, trimmed_start);
            occurrences.push(ReferenceOccurrence {
                family: family.to_string(),
                wrapper: wrapper.to_string(),
                target_expression,
                line,
                column,
            });
        }
        // Resume immediately after the wrapper identifier, not after its
        // closing angle. Nested qualifying wrappers are independent reference
        // occurrences and must not disappear inside an outer target type.
    }

    occurrences.sort_by(|left, right| {
        (
            &left.family,
            &left.wrapper,
            left.line,
            left.column,
            &left.target_expression,
        )
            .cmp(&(
                &right.family,
                &right.wrapper,
                right.line,
                right.column,
                &right.target_expression,
            ))
    });
    if occurrences.windows(2).any(|pair| pair[0] == pair[1]) {
        let duplicate = occurrences
            .windows(2)
            .find(|pair| pair[0] == pair[1])
            .map(|pair| &pair[0])
            .expect("duplicate predicate guarantees one pair");
        return Err(ReferenceCensusError {
            code: "reference_occurrence_duplicate",
            line: duplicate.line,
            column: duplicate.column,
        });
    }

    let mut grouped: BTreeMap<String, Vec<ReferenceOccurrence>> = BTreeMap::new();
    for occurrence in &occurrences {
        grouped
            .entry(occurrence.family.clone())
            .or_default()
            .push(occurrence.clone());
    }
    let targets: Vec<ReferenceTarget> = grouped
        .into_iter()
        .map(|(family, rows)| {
            let wrappers: BTreeSet<String> = rows.iter().map(|row| row.wrapper.clone()).collect();
            let target_expressions: BTreeSet<String> = rows
                .iter()
                .map(|row| row.target_expression.clone())
                .collect();
            ReferenceTarget {
                family,
                wrappers: wrappers.into_iter().collect(),
                target_expressions: target_expressions.into_iter().collect(),
                occurrences: rows,
            }
        })
        .collect();

    let mut target_transcript = String::new();
    for target in &targets {
        writeln!(&mut target_transcript, "{}", target.family)
            .expect("writing to String cannot fail");
    }
    let mut occurrence_transcript = String::new();
    for occurrence in &occurrences {
        writeln!(
            &mut occurrence_transcript,
            "{}|{}|{}|{}|{}",
            occurrence.family,
            occurrence.wrapper,
            occurrence.target_expression,
            occurrence.line,
            occurrence.column,
        )
        .expect("writing to String cannot fail");
    }

    Ok(ReferenceCensus {
        target_count: targets.len(),
        target_ids_sha256: sha256_hex(target_transcript.as_bytes()),
        occurrence_count: occurrences.len(),
        occurrence_transcript_sha256: sha256_hex(occurrence_transcript.as_bytes()),
        targets,
        occurrences,
    })
}

fn qualifying_wrapper(wrapper: &str) -> bool {
    matches!(wrapper, "StrongRef" | "CertifiedRemoteStrongRef")
        || (wrapper.starts_with("Conditional") && wrapper.ends_with("Ref"))
}

fn identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn identifier_continue(byte: u8) -> bool {
    identifier_start(byte) || byte.is_ascii_digit()
}

fn skip_unicode_whitespace(text: &str, start: usize) -> usize {
    let mut cursor = start;
    for character in text[start..].chars() {
        if !character.is_whitespace() {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

fn matching_angle(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().copied().enumerate().skip(open) {
        match byte {
            b'<' => depth = depth.checked_add(1)?,
            b'>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn top_level_alternatives(
    text: &str,
    start: usize,
    end: usize,
    source_map: &SourceMap,
) -> Result<Vec<(usize, usize)>, ReferenceCensusError> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut alternative_start = start;
    let mut delimiters = Vec::new();
    for (index, byte) in bytes.iter().copied().enumerate().take(end).skip(start) {
        match byte {
            b'<' | b'{' | b'[' | b'(' => delimiters.push(byte),
            b'>' | b'}' | b']' | b')' => {
                let expected = match byte {
                    b'>' => b'<',
                    b'}' => b'{',
                    b']' => b'[',
                    b')' => b'(',
                    _ => unreachable!("closed match over delimiter bytes"),
                };
                if delimiters.pop() != Some(expected) {
                    return Err(nesting_error(text, source_map, index));
                }
            }
            b'|' if delimiters.is_empty() => {
                out.push((alternative_start, index));
                alternative_start = index + 1;
            }
            _ => {}
        }
    }
    if !delimiters.is_empty() {
        return Err(nesting_error(text, source_map, end.saturating_sub(1)));
    }
    out.push((alternative_start, end));
    Ok(out)
}

fn nesting_error(text: &str, source_map: &SourceMap, offset: usize) -> ReferenceCensusError {
    let (line, column) = source_map.line_column(text, offset);
    ReferenceCensusError {
        code: "reference_target_unbalanced",
        line,
        column,
    }
}

fn first_non_whitespace(text: &str, start: usize, end: usize) -> usize {
    text[start..end]
        .char_indices()
        .find_map(|(offset, character)| (!character.is_whitespace()).then_some(start + offset))
        .unwrap_or(end)
}

fn last_non_whitespace(text: &str, start: usize, end: usize) -> usize {
    text[start..end]
        .char_indices()
        .rev()
        .find_map(|(offset, character)| {
            (!character.is_whitespace()).then_some(start + offset + character.len_utf8())
        })
        .unwrap_or(start)
}

fn normalize_whitespace(value: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
            }
            out.push(character);
            pending_space = false;
        }
    }
    out
}

fn leading_family(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_uppercase) {
        return None;
    }
    let end = bytes
        .iter()
        .position(|byte| !identifier_continue(*byte))
        .unwrap_or(bytes.len());
    Some(&value[..end])
}

fn schema_metavariable(family: &str) -> bool {
    matches!(
        family,
        "A" | "B"
            | "T"
            | "Role"
            | "Kind"
            | "Enum"
            | "Local"
            | "Meta"
            | "Shard"
            | "ExactRegisteredInput"
    )
}

struct SourceMap {
    byte_len: usize,
    line_starts: Vec<usize>,
}

impl SourceMap {
    fn new(bytes: &[u8]) -> Self {
        let mut line_starts = vec![0];
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' {
                line_starts.push(index.saturating_add(1));
            }
        }
        Self {
            byte_len: bytes.len(),
            line_starts,
        }
    }

    fn byte_line_column(&self, offset: usize) -> (usize, usize) {
        let bounded = offset.min(self.byte_len);
        let line_index = self
            .line_starts
            .partition_point(|line_start| *line_start <= bounded)
            .saturating_sub(1);
        let line_start = self.line_starts.get(line_index).copied().unwrap_or(0);
        (
            line_index.saturating_add(1),
            bounded.saturating_sub(line_start).saturating_add(1),
        )
    }

    fn utf8_prefix_line_column(&self, bytes: &[u8], offset: usize) -> (usize, usize) {
        let bounded = offset.min(self.byte_len);
        let (line, _) = self.byte_line_column(bounded);
        let line_start = self
            .line_starts
            .get(line.saturating_sub(1))
            .copied()
            .unwrap_or(0);
        let column = std::str::from_utf8(&bytes[line_start..bounded])
            .map_or(1, |prefix| prefix.chars().count().saturating_add(1));
        (line, column)
    }

    /// One-based Unicode-scalar column, matching the structural source census.
    fn line_column(&self, text: &str, offset: usize) -> (usize, usize) {
        let bounded = offset.min(self.byte_len);
        let (line, _) = self.byte_line_column(bounded);
        let line_start = self
            .line_starts
            .get(line.saturating_sub(1))
            .copied()
            .unwrap_or(0);
        let column = text[line_start..bounded].chars().count().saturating_add(1);
        (line, column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_top_level_unions_and_preserves_nested_expressions() {
        let source = b"StrongRef<Alpha<Role>| Beta<{x:u8|u16}> |T>\nCertifiedRemoteStrongRef<Gamma::Ready>\nConditionalCoordinateRef<DeltaBlockVersion>\n";
        let census = census_plan_references(source).expect("census succeeds");
        let families: Vec<_> = census
            .targets
            .iter()
            .map(|target| target.family.as_str())
            .collect();
        assert_eq!(families, ["Alpha", "Beta", "DeltaBlockVersion", "Gamma"]);
        assert_eq!(census.occurrence_count, 4);
        assert_eq!(census.occurrences[0].target_expression, "Alpha<Role>");
        assert_eq!(census.occurrences[1].target_expression, "Beta<{x:u8|u16}>");
        assert_eq!(census.occurrences[2].line, 3);
        assert_eq!(census.occurrences[2].column, 26);
    }

    #[test]
    fn ignores_nonqualifying_wrappers_and_schema_metavariables() {
        let source = b"StrongCiphertextRef<Ciphertext> PreBootstrapArtifactRef<Plan> RegisteredStrongRef<Row> StrongRef<T> ConditionalThingRef<Role>";
        let census = census_plan_references(source).expect("census succeeds");
        assert!(census.targets.is_empty());
        assert_eq!(
            census.target_ids_sha256,
            sha256_hex(b""),
            "empty transcript remains deterministic"
        );
    }

    #[test]
    fn occurrence_transcript_distinguishes_duplicate_text_by_position() {
        let census = census_plan_references(b"StrongRef<Alpha> StrongRef<Alpha>\n")
            .expect("two source occurrences are legal");
        assert_eq!(census.target_count, 1);
        assert_eq!(census.occurrence_count, 2);
        assert_ne!(census.occurrences[0].column, census.occurrences[1].column);
    }

    #[test]
    fn nested_wrappers_are_independent_occurrences() {
        let census = census_plan_references(
            b"StrongRef<Envelope<StrongRef<Inner>>> ConditionalThingRef\xC2\xA0<Leaf>\n",
        )
        .expect("nested and Unicode-spaced wrappers parse");
        let families: Vec<_> = census
            .targets
            .iter()
            .map(|target| target.family.as_str())
            .collect();
        assert_eq!(families, ["Envelope", "Inner", "Leaf"]);
        assert_eq!(census.occurrence_count, 3);
    }

    #[test]
    fn unicode_target_whitespace_is_trimmed_before_coordinates() {
        let census = census_plan_references("StrongRef<\u{00a0}Alpha\u{00a0}>".as_bytes())
            .expect("Unicode target whitespace is accepted");
        assert_eq!(census.occurrences[0].target_expression, "Alpha");
        assert_eq!(census.occurrences[0].column, 12);

        let empty = census_plan_references("StrongRef<\u{00a0}>".as_bytes())
            .expect_err("Unicode-whitespace-only target is empty");
        assert_eq!(empty.code, "reference_target_empty");
    }

    #[test]
    fn malformed_qualifying_wrappers_fail_closed() {
        for (source, code) in [
            (&b"StrongRef<Alpha"[..], "reference_wrapper_unclosed"),
            (&b"StrongRef<Alpha|>"[..], "reference_target_empty"),
            (&b"StrongRef<[Alpha]>"[..], "reference_target_unparsed"),
            (&b"StrongRef<Alpha}>"[..], "reference_target_unbalanced"),
            (&b"StrongRef<Alpha{[x)}>"[..], "reference_target_unbalanced"),
            (&b"StrongRef<Alpha([x)]>"[..], "reference_target_unbalanced"),
        ] {
            let error = census_plan_references(source).expect_err("malformed reference fails");
            assert_eq!(error.code, code);
        }
    }

    #[test]
    fn crlf_and_invalid_utf8_fail_closed_with_locations() {
        let crlf = census_plan_references(b"StrongRef<Alpha>\r\n").expect_err("CRLF rejected");
        assert_eq!(crlf.code, "reference_source_not_lf");
        assert_eq!((crlf.line, crlf.column), (1, 17));

        let utf8 = census_plan_references(&[b'\n', 0xff]).expect_err("invalid UTF-8 rejected");
        assert_eq!(utf8.code, "reference_source_not_utf8");
        assert_eq!((utf8.line, utf8.column), (2, 1));

        let unicode_prefix = census_plan_references(&[0xc3, 0xa9, 0xff])
            .expect_err("invalid UTF-8 after a multibyte scalar is rejected");
        assert_eq!((unicode_prefix.line, unicode_prefix.column), (1, 2));
    }

    #[test]
    fn committed_plan_reference_census_is_pinned() {
        let source =
            include_bytes!("../../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md");
        let census = census_plan_references(source).expect("committed plan census succeeds");
        assert_eq!(census.target_count, 813);
        assert_eq!(
            census.target_ids_sha256,
            "84276b6d97342e9ec1619424ddacb5b429e98e1862e03359afc837b65bb3392e"
        );
        assert_eq!(census.occurrence_count, 2_455);
        assert_eq!(
            census.occurrence_transcript_sha256,
            "c715512f6adf746157692028a3fdf001dd5ce4f5dfb54b4d87a2f1957d92298a"
        );
    }
}
