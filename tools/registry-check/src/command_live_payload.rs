//! Fail-closed binding between LIVE command-contract input schemas and the
//! payload types of their inhabitable Rust command-union arms.
//!
//! Bead: fgdb-5uw2.
//!
//! `command_contracts::validate` proves that every LIVE registry row has one
//! inventory declaration, that the declared arm and handler symbols occur in
//! the Rust source, and that the handler match is exhaustive. That is necessary
//! but not sufficient: a registry row can name a different registered input
//! schema while retaining the same real arm and handler. This module closes
//! that false-green seam by parsing the declared enum arm and requiring its
//! single tuple payload to equal the row's canonical `input_schema_id`.
//!
//! The parser is intentionally small and std-only. It masks comments and Rust
//! string/character literals before structural scanning, matches identifiers
//! rather than substrings, balances enum/variant delimiters, ignores unrelated
//! variant shapes, and fails closed on an unsupported shape for the target arm.
//! It is a verifier, not a Rust front end.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::command_contracts::{
    ContractRegistry, LIVE_HANDLER_SOURCE_PATH, load_from_repo as load_contracts_from_repo,
};

const LIVE_HANDLER_INVENTORY_NAME: &str = "LIVE_LOCAL_SEMANTIC_HANDLER_INVENTORY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub code: String,
    pub subject: String,
    pub message: String,
}

impl Violation {
    fn new(code: &str, subject: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            subject: subject.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InventoryDeclaration {
    contract_id: String,
    arm_name: String,
}

pub fn validate_repo(root: &Path) -> Result<Vec<Violation>, String> {
    let registry = load_contracts_from_repo(root).map_err(|error| error.to_string())?;
    let source_path = root.join(LIVE_HANDLER_SOURCE_PATH);
    let source = fs::read_to_string(&source_path)
        .map_err(|error| format!("{}: {error}", source_path.display()))?;
    Ok(validate_sources(&registry, &source))
}

pub fn validate_sources(registry: &ContractRegistry, source: &str) -> Vec<Violation> {
    let declarations = match parse_inventory(source) {
        Ok(declarations) => declarations,
        Err(message) => {
            return vec![Violation::new(
                "live_payload_inventory_parse",
                LIVE_HANDLER_INVENTORY_NAME,
                message,
            )];
        }
    };

    let mut violations = Vec::new();
    let mut by_contract = BTreeMap::<&str, &InventoryDeclaration>::new();
    for declaration in &declarations {
        if by_contract
            .insert(declaration.contract_id.as_str(), declaration)
            .is_some()
        {
            violations.push(Violation::new(
                "live_payload_inventory_duplicate",
                &declaration.contract_id,
                "live handler inventory declares the contract more than once",
            ));
        }
    }

    let masked = mask_non_code(source.as_bytes());
    for contract in registry
        .contracts
        .iter()
        .filter(|contract| contract.status == "live")
    {
        let subject = contract.command_contract_id.as_str();
        let Some(declaration) = by_contract.get(subject).copied() else {
            violations.push(Violation::new(
                "live_payload_inventory_missing",
                subject,
                "LIVE command contract has no handler-inventory declaration",
            ));
            continue;
        };

        let payload = match enum_arm_payload(
            &masked,
            &contract.outer_command_union,
            &declaration.arm_name,
        ) {
            Ok(payload) => payload,
            Err((code, message)) => {
                violations.push(Violation::new(code, subject, message));
                continue;
            }
        };
        let expected = normalize_schema_id(&contract.input_schema_id);
        if payload != expected {
            violations.push(Violation::new(
                "live_payload_schema_mismatch",
                subject,
                format!(
                    "{}.{} payload type {:?} does not equal input_schema_id {:?}",
                    contract.outer_command_union, declaration.arm_name, payload, expected
                ),
            ));
        }
    }

    violations
}

fn parse_inventory(source: &str) -> Result<Vec<InventoryDeclaration>, String> {
    let start = source
        .find(LIVE_HANDLER_INVENTORY_NAME)
        .ok_or_else(|| format!("source has no {LIVE_HANDLER_INVENTORY_NAME} declaration"))?;
    let tail = &source[start..];
    let end = tail
        .find("];")
        .ok_or_else(|| format!("unterminated {LIVE_HANDLER_INVENTORY_NAME}"))?;
    let quoted = tail[..end]
        .split('"')
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, field)| field.to_string())
        .collect::<Vec<_>>();
    if !quoted.len().is_multiple_of(3) {
        return Err(format!(
            "{LIVE_HANDLER_INVENTORY_NAME} must contain contract-id, handler-symbol, union-arm triples"
        ));
    }
    let mut declarations = Vec::with_capacity(quoted.len() / 3);
    for triple in quoted.chunks_exact(3) {
        declarations.push(InventoryDeclaration {
            contract_id: triple[0].clone(),
            arm_name: triple[2].clone(),
        });
    }
    if declarations.is_empty() {
        return Err(format!(
            "{LIVE_HANDLER_INVENTORY_NAME} contains no contract declarations"
        ));
    }
    Ok(declarations)
}

fn enum_arm_payload(
    masked: &[u8],
    union_name: &str,
    target_arm: &str,
) -> Result<String, (&'static str, String)> {
    let enum_name = source_type_name(union_name);
    let (body_start, body_end) = find_enum_body(masked, enum_name).ok_or_else(|| {
        (
            "live_payload_union_missing",
            format!("Rust source has no enum declaration for {enum_name:?}"),
        )
    })?;

    let mut matches = Vec::new();
    for (start, end) in split_top_level(masked, body_start, body_end, b',') {
        let Some((arm_name, after_name)) = variant_name(masked, start, end)? else {
            continue;
        };
        if arm_name != target_arm {
            continue;
        }
        matches.push(target_variant_payload(
            masked,
            after_name,
            end,
            target_arm,
        )?);
    }
    match matches.len() {
        0 => Err((
            "live_payload_arm_missing",
            format!("enum {union_name:?} has no arm {target_arm:?}"),
        )),
        1 => Ok(matches.remove(0)),
        count => Err((
            "live_payload_arm_duplicate",
            format!("enum {union_name:?} contains {count} arms named {target_arm:?}"),
        )),
    }
}

fn source_type_name(type_name: &str) -> &str {
    type_name
        .split('<')
        .next()
        .unwrap_or(type_name)
        .rsplit("::")
        .next()
        .unwrap_or(type_name)
        .trim()
}

fn find_enum_body(masked: &[u8], enum_name: &str) -> Option<(usize, usize)> {
    let mut cursor = 0usize;
    while let Some((token_start, token_end)) = next_identifier(masked, cursor) {
        cursor = token_end;
        if &masked[token_start..token_end] != b"enum" {
            continue;
        }
        let Some((name_start, name_end)) = next_identifier(masked, cursor) else {
            return None;
        };
        cursor = name_end;
        if &masked[name_start..name_end] != enum_name.as_bytes() {
            continue;
        }
        let open = masked[name_end..]
            .iter()
            .position(|byte| *byte == b'{')
            .map(|offset| name_end + offset)?;
        let close = matching_delimiter(masked, open, b'{', b'}')?;
        return Some((open + 1, close));
    }
    None
}

fn variant_name(
    masked: &[u8],
    start: usize,
    end: usize,
) -> Result<Option<(String, usize)>, (&'static str, String)> {
    let mut cursor = skip_whitespace(masked, start, end);
    while cursor < end && masked[cursor] == b'#' {
        cursor += 1;
        if cursor < end && masked[cursor] == b'!' {
            cursor += 1;
        }
        cursor = skip_whitespace(masked, cursor, end);
        if cursor >= end || masked[cursor] != b'[' {
            return Err((
                "live_payload_variant_parse",
                "malformed attribute before command-union arm".to_string(),
            ));
        }
        let Some(close) = matching_delimiter(masked, cursor, b'[', b']') else {
            return Err((
                "live_payload_variant_parse",
                "unterminated attribute before command-union arm".to_string(),
            ));
        };
        cursor = skip_whitespace(masked, close + 1, end);
    }
    if cursor >= end {
        return Ok(None);
    }
    let Some((name_start, name_end)) = next_identifier_in(masked, cursor, end) else {
        return Ok(None);
    };
    if name_start != cursor {
        return Err((
            "live_payload_variant_parse",
            "command-union arm does not begin with an identifier".to_string(),
        ));
    }
    let arm_name = std::str::from_utf8(&masked[name_start..name_end])
        .map_err(|error| {
            (
                "live_payload_variant_parse",
                format!("command-union arm identifier is not UTF-8: {error}"),
            )
        })?
        .to_string();
    Ok(Some((arm_name, name_end)))
}

fn target_variant_payload(
    masked: &[u8],
    after_name: usize,
    end: usize,
    arm_name: &str,
) -> Result<String, (&'static str, String)> {
    let cursor = skip_whitespace(masked, after_name, end);
    if cursor >= end || masked[cursor] != b'(' {
        return Err((
            "live_payload_variant_shape",
            format!("command-union arm {arm_name:?} is not a single-payload tuple variant"),
        ));
    }
    let Some(close) = matching_delimiter(masked, cursor, b'(', b')') else {
        return Err((
            "live_payload_variant_parse",
            format!("command-union arm {arm_name:?} has an unterminated tuple payload"),
        ));
    };
    if skip_whitespace(masked, close + 1, end) != end {
        return Err((
            "live_payload_variant_shape",
            format!("command-union arm {arm_name:?} carries tokens after its tuple payload"),
        ));
    }

    let fields = split_top_level(masked, cursor + 1, close, b',')
        .into_iter()
        .filter_map(|(field_start, field_end)| {
            let first = skip_whitespace(masked, field_start, field_end);
            let last = trim_whitespace_end(masked, first, field_end);
            (first < last).then_some((first, last))
        })
        .collect::<Vec<_>>();
    if fields.len() != 1 {
        return Err((
            "live_payload_variant_shape",
            format!(
                "command-union arm {arm_name:?} has {} tuple payload fields; exactly one is required",
                fields.len()
            ),
        ));
    }
    normalize_code(&masked[fields[0].0..fields[0].1])
}

fn normalize_schema_id(schema_id: &str) -> String {
    schema_id
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn normalize_code(code: &[u8]) -> Result<String, (&'static str, String)> {
    let text = std::str::from_utf8(code).map_err(|error| {
        (
            "live_payload_variant_parse",
            format!("command-union arm payload is not UTF-8: {error}"),
        )
    })?;
    Ok(text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect())
}

fn split_top_level(
    bytes: &[u8],
    start: usize,
    end: usize,
    separator: u8,
) -> Vec<(usize, usize)> {
    let mut pieces = Vec::new();
    let mut piece_start = start;
    let (mut paren, mut bracket, mut brace, mut angle) = (0usize, 0usize, 0usize, 0usize);
    for index in start..end {
        match bytes[index] {
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            b'{' => brace += 1,
            b'}' => brace = brace.saturating_sub(1),
            b'<' => angle += 1,
            b'>' if angle > 0 => angle -= 1,
            byte
                if byte == separator
                    && paren == 0
                    && bracket == 0
                    && brace == 0
                    && angle == 0 =>
            {
                pieces.push((piece_start, index));
                piece_start = index + 1;
            }
            _ => {}
        }
    }
    pieces.push((piece_start, end));
    pieces
}

fn matching_delimiter(bytes: &[u8], open: usize, left: u8, right: u8) -> Option<usize> {
    if bytes.get(open).copied() != Some(left) {
        return None;
    }
    let mut depth = 0usize;
    for (offset, byte) in bytes[open..].iter().copied().enumerate() {
        if byte == left {
            depth += 1;
        } else if byte == right {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(open + offset);
            }
        }
    }
    None
}

fn next_identifier(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    next_identifier_in(bytes, start, bytes.len())
}

fn next_identifier_in(bytes: &[u8], mut cursor: usize, end: usize) -> Option<(usize, usize)> {
    while cursor < end && !is_identifier_start(bytes[cursor]) {
        cursor += 1;
    }
    if cursor >= end {
        return None;
    }
    let start = cursor;
    cursor += 1;
    while cursor < end && is_identifier_continue(bytes[cursor]) {
        cursor += 1;
    }
    Some((start, cursor))
}

fn is_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn skip_whitespace(bytes: &[u8], mut cursor: usize, end: usize) -> usize {
    while cursor < end && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    cursor
}

fn trim_whitespace_end(bytes: &[u8], start: usize, mut end: usize) -> usize {
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    end
}

fn mask_non_code(source: &[u8]) -> Vec<u8> {
    let mut masked = source.to_vec();
    let mut index = 0usize;
    while index < source.len() {
        if source[index..].starts_with(b"//") {
            let end = source[index..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(source.len(), |offset| index + offset);
            mask_range(&mut masked, index, end);
            index = end;
            continue;
        }
        if source[index..].starts_with(b"/*") {
            let mut cursor = index + 2;
            let mut depth = 1usize;
            while cursor < source.len() && depth > 0 {
                if source[cursor..].starts_with(b"/*") {
                    depth += 1;
                    cursor += 2;
                } else if source[cursor..].starts_with(b"*/") {
                    depth -= 1;
                    cursor += 2;
                } else {
                    cursor += 1;
                }
            }
            mask_range(&mut masked, index, cursor);
            index = cursor;
            continue;
        }
        if let Some((content_start, hashes)) = raw_string_start(source, index) {
            let mut cursor = content_start;
            let mut end = source.len();
            while cursor < source.len() {
                if source[cursor] == b'"'
                    && source
                        .get(cursor + 1..cursor + 1 + hashes)
                        .is_some_and(|tail| tail.iter().all(|byte| *byte == b'#'))
                {
                    end = cursor + 1 + hashes;
                    break;
                }
                cursor += 1;
            }
            mask_range(&mut masked, index, end);
            index = end;
            continue;
        }
        let string_quote = if source[index] == b'"' {
            Some(index)
        } else if matches!(source[index], b'b' | b'c')
            && source.get(index + 1) == Some(&b'"')
        {
            Some(index + 1)
        } else {
            None
        };
        if let Some(quote) = string_quote {
            let end = quoted_string_end(source, quote).unwrap_or(source.len());
            mask_range(&mut masked, index, end);
            index = end;
            continue;
        }
        let char_quote = if source[index] == b'\'' {
            Some(index)
        } else if source[index] == b'b' && source.get(index + 1) == Some(&b'\'') {
            Some(index + 1)
        } else {
            None
        };
        if let Some(quote) = char_quote
            && let Some(end) = char_literal_end(source, quote)
        {
            mask_range(&mut masked, index, end);
            index = end;
            continue;
        }
        index += 1;
    }
    masked
}

fn raw_string_start(source: &[u8], index: usize) -> Option<(usize, usize)> {
    let mut cursor = index;
    if source
        .get(cursor)
        .is_some_and(|byte| matches!(*byte, b'b' | b'c'))
    {
        cursor += 1;
    }
    if source.get(cursor) != Some(&b'r') {
        return None;
    }
    cursor += 1;
    let hash_start = cursor;
    while source.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    if source.get(cursor) != Some(&b'"') {
        return None;
    }
    Some((cursor + 1, cursor - hash_start))
}

fn quoted_string_end(source: &[u8], quote: usize) -> Option<usize> {
    let mut cursor = quote + 1;
    let mut escaped = false;
    while cursor < source.len() {
        let byte = source[cursor];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return Some(cursor + 1);
        }
        cursor += 1;
    }
    None
}

fn char_literal_end(source: &[u8], quote: usize) -> Option<usize> {
    let mut cursor = quote + 1;
    let first = *source.get(cursor)?;
    if first == b'\\' {
        cursor += 1;
        match *source.get(cursor)? {
            b'x' => cursor += 3,
            b'u' => {
                cursor += 1;
                if source.get(cursor) != Some(&b'{') {
                    return None;
                }
                cursor += 1;
                while source.get(cursor).is_some_and(|byte| *byte != b'}') {
                    if source[cursor] == b'\n' {
                        return None;
                    }
                    cursor += 1;
                }
                cursor += 1;
            }
            _ => cursor += 1,
        }
    } else {
        let text = std::str::from_utf8(source.get(cursor..)?).ok()?;
        let character = text.chars().next()?;
        if character == '\n' || character == '\r' || character == '\'' {
            return None;
        }
        cursor += character.len_utf8();
    }
    (source.get(cursor) == Some(&b'\'')).then_some(cursor + 1)
}

fn mask_range(masked: &mut [u8], start: usize, end: usize) {
    let end = end.min(masked.len());
    for byte in &mut masked[start..end] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{enum_arm_payload, mask_non_code, validate_repo, validate_sources};
    use crate::command_contracts::{LIVE_HANDLER_SOURCE_PATH, load_from_repo};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn live_command_payload_bindings_match_registry() {
        let violations = validate_repo(&repo_root())
            .unwrap_or_else(|error| panic!("live payload checker failed to load: {error}"));
        assert!(
            violations.is_empty(),
            "live command payload binding violations: {violations:#?}"
        );
    }

    #[test]
    fn wrong_registry_input_schema_is_rejected() {
        let root = repo_root();
        let mut registry = load_from_repo(&root)
            .unwrap_or_else(|error| panic!("command contracts failed to load: {error}"));
        let target_id = "cc:local:local-autocommit-write-spec";
        let Some(target) = registry
            .contracts
            .iter_mut()
            .find(|contract| contract.command_contract_id == target_id)
        else {
            panic!("live mutation target moved: {target_id}");
        };
        assert_eq!(target.status, "live", "mutation target is no longer live");
        target.input_schema_id = "LocalBeginReservationSpec".to_string();

        let source_path = root.join(LIVE_HANDLER_SOURCE_PATH);
        let source = fs::read_to_string(&source_path)
            .unwrap_or_else(|error| panic!("{}: {error}", source_path.display()));
        let violations = validate_sources(&registry, &source);
        assert!(
            violations.iter().any(|violation| {
                violation.code == "live_payload_schema_mismatch"
                    && violation.subject == target_id
            }),
            "wrong input-schema mutation was not rejected: {violations:#?}"
        );
    }

    #[test]
    fn unrelated_variant_shapes_and_lifetimes_do_not_confuse_target_parser() {
        let source = br#"
            type Borrowed<'a> = &'a str;
            const BRACE: char = '{';
            enum LocalSemanticCommand {
                Unit,
                Struct { value: Borrowed<'static> },
                WriteBatch(LocalAutocommitWriteSpec),
            }
        "#;
        let masked = mask_non_code(source);
        let payload = enum_arm_payload(&masked, "LocalSemanticCommand", "WriteBatch")
            .unwrap_or_else(|error| panic!("target arm did not parse: {error:?}"));
        assert_eq!(payload, "LocalAutocommitWriteSpec");
    }
}
