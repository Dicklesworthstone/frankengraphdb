//! Artifact-pinned canonical text semantics for STRICT_PORTABLE.
//!
//! UCS_BASIC compares UTF-8 bytes directly. Every non-binary binding names
//! the exact Unicode, normalization, segmentation, and collation artifacts
//! used to produce its sort keys. Construction and decoding validate those
//! identities through a caller-supplied [`CollationResolver`] capability;
//! this module never consults the host locale or host Unicode tables.
//!
//! Non-binary collation is intentionally not implemented here. Its sort key
//! is derived by the resolver from the exact binding plus text, then stored as
//! the primary ordering input. Original UTF-8 bytes are the deterministic
//! tie-breaker, preserving `cmp(a, b) == Equal` iff `a == b`.

use crate::ids::ObjectId;
use crate::scalar::MAX_SCALAR_PAYLOAD;
use std::cmp::Ordering;

/// Maximum UTF-8 payload accepted by the canonical scalar profile.
pub const MAX_CANONICAL_TEXT_BYTES: usize = 64 * 1024 * 1024;

/// Maximum canonical collation-key payload accepted by the profile.
pub const MAX_CANONICAL_SORT_KEY_BYTES: usize = 64 * 1024 * 1024;

const TEXT_ENCODING_VERSION: u8 = 1;
const UCS_BASIC_TAG: u8 = 0;
const NON_BINARY_TAG: u8 = 1;
const OBJECT_ID_BYTES: usize = 32;

/// The four content-addressed inputs needed for non-binary text semantics.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NonBinaryTextBinding {
    pub unicode_data_oid: ObjectId,
    pub normalization_oid: ObjectId,
    pub segmentation_oid: ObjectId,
    pub collation_oid: ObjectId,
}

impl NonBinaryTextBinding {
    pub const fn new(
        unicode_data_oid: ObjectId,
        normalization_oid: ObjectId,
        segmentation_oid: ObjectId,
        collation_oid: ObjectId,
    ) -> Self {
        Self {
            unicode_data_oid,
            normalization_oid,
            segmentation_oid,
            collation_oid,
        }
    }

    /// Fails closed unless all four pinned artifacts are available.
    pub fn validate_artifacts<R: CollationResolver + ?Sized>(
        &self,
        resolver: &R,
    ) -> Result<(), CanonicalTextError> {
        for (role, oid) in self.artifacts() {
            if !resolver.artifact_available(oid) {
                return Err(CanonicalTextError::MissingArtifact {
                    role,
                    object_id: *oid,
                });
            }
        }
        Ok(())
    }

    fn artifacts(&self) -> [(TextArtifactRole, &ObjectId); 4] {
        [
            (TextArtifactRole::UnicodeData, &self.unicode_data_oid),
            (TextArtifactRole::Normalization, &self.normalization_oid),
            (TextArtifactRole::Segmentation, &self.segmentation_oid),
            (TextArtifactRole::Collation, &self.collation_oid),
        ]
    }
}

/// Binary UCS_BASIC or an exact content-addressed non-binary profile.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TextBinding {
    UcsBasic,
    NonBinary(NonBinaryTextBinding),
}

/// Trusted capability for artifact-pinned non-binary collation.
///
/// The two-phase interface makes the exact key length available for bounds
/// checking before this module allocates. Implementations must derive bytes
/// solely from `binding`, `text`, and the named artifacts. They must not read
/// host locale state.
pub trait CollationResolver {
    /// Reports whether one exact content-addressed artifact is available.
    fn artifact_available(&self, object_id: &ObjectId) -> bool;

    /// Reports the exact number of bytes the canonical key will contain.
    fn canonical_sort_key_len(
        &self,
        binding: &NonBinaryTextBinding,
        text: &str,
    ) -> Result<usize, CollationResolverError>;

    /// Writes the canonical key into an exactly sized output slice and
    /// returns the number of bytes written.
    fn write_canonical_sort_key(
        &self,
        binding: &NonBinaryTextBinding,
        text: &str,
        output: &mut [u8],
    ) -> Result<usize, CollationResolverError>;

    /// Verifies a candidate key without materializing a second full key.
    /// Implementations must perform exact byte comparison using bounded
    /// additional storage; probabilistic hashes are not admissible.
    fn canonical_sort_key_matches(
        &self,
        binding: &NonBinaryTextBinding,
        text: &str,
        candidate: &[u8],
    ) -> Result<bool, CollationResolverError>;
}

/// Resolver-specific failure, represented by a stable implementation-defined
/// code rather than an unbounded diagnostic string.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CollationResolverError {
    pub code: u32,
}

impl CollationResolverError {
    pub const fn new(code: u32) -> Self {
        Self { code }
    }
}

impl std::fmt::Display for CollationResolverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "collation resolver rejected request with code {}",
            self.code
        )
    }
}

impl std::error::Error for CollationResolverError {}

/// Canonical UTF-8 text plus its complete comparison binding.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CanonicalText {
    text: String,
    binding: TextBinding,
    sort_key: Option<Vec<u8>>,
}

impl CanonicalText {
    /// Constructs binary UCS_BASIC text. Ordering is raw UTF-8 byte order.
    pub fn new_ucs_basic(text: &str) -> Result<Self, CanonicalTextError> {
        check_len(TextField::Text, text.len(), MAX_CANONICAL_TEXT_BYTES)?;
        check_ordered_scalar_payload(text.len(), None)?;
        Ok(Self {
            text: copy_string(text)?,
            binding: TextBinding::UcsBasic,
            sort_key: None,
        })
    }

    /// Crate-internal zero-copy admission for a decoded UCS_BASIC value.
    pub(crate) fn from_owned_ucs_basic(text: String) -> Result<Self, CanonicalTextError> {
        check_len(TextField::Text, text.len(), MAX_CANONICAL_TEXT_BYTES)?;
        check_ordered_scalar_payload(text.len(), None)?;
        Ok(Self {
            text,
            binding: TextBinding::UcsBasic,
            sort_key: None,
        })
    }

    /// Constructs artifact-pinned non-binary text.
    ///
    /// Artifact availability and both payload bounds are checked before this
    /// function allocates owned text or key storage. Only the resolver can
    /// derive the canonical sort key.
    pub fn new_non_binary<R: CollationResolver + ?Sized>(
        text: &str,
        binding: NonBinaryTextBinding,
        resolver: &R,
    ) -> Result<Self, CanonicalTextError> {
        check_len(TextField::Text, text.len(), MAX_CANONICAL_TEXT_BYTES)?;
        let sort_key = resolve_sort_key(binding, text, resolver)?;
        Self::from_resolved_non_binary(text, binding, sort_key)
    }

    /// Crate-internal admission for the ordered scalar decoder. The encoded
    /// key is never trusted: the resolver derives it again from the pinned
    /// artifacts and exact text before the owned value is admitted.
    pub(crate) fn from_ordered_scalar_parts<R: CollationResolver + ?Sized>(
        text: String,
        binding: NonBinaryTextBinding,
        encoded_sort_key: Vec<u8>,
        resolver: &R,
    ) -> Result<Self, CanonicalTextError> {
        check_len(TextField::Text, text.len(), MAX_CANONICAL_TEXT_BYTES)?;
        check_len(
            TextField::SortKey,
            encoded_sort_key.len(),
            MAX_CANONICAL_SORT_KEY_BYTES,
        )?;
        check_ordered_scalar_payload(text.len(), Some(encoded_sort_key.len()))?;
        binding.validate_artifacts(resolver)?;
        let matches = resolver
            .canonical_sort_key_matches(&binding, &text, &encoded_sort_key)
            .map_err(CanonicalTextError::ResolverFailure)?;
        if !matches {
            return Err(CanonicalTextError::SortKeyMismatch {
                encoded_len: encoded_sort_key.len(),
                resolved_len: resolver
                    .canonical_sort_key_len(&binding, &text)
                    .map_err(CanonicalTextError::ResolverFailure)?,
            });
        }
        Ok(Self {
            text,
            binding: TextBinding::NonBinary(binding),
            sort_key: Some(encoded_sort_key),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub const fn binding(&self) -> &TextBinding {
        &self.binding
    }

    pub fn canonical_sort_key(&self) -> Option<&[u8]> {
        self.sort_key.as_deref()
    }

    /// Bytes defining collation equivalence under this value's pinned
    /// binding. Scalar `Eq` remains exact value identity (and therefore keeps
    /// original UTF-8 spelling); collation-aware predicates and constraints
    /// compare both [`Self::binding`] and these bytes explicitly.
    pub fn collation_equivalence_bytes(&self) -> &[u8] {
        match &self.sort_key {
            Some(sort_key) => sort_key,
            None => self.text.as_bytes(),
        }
    }

    /// Tests collation equivalence without conflating it with exact scalar
    /// equality. Different original spellings may be collation-equivalent.
    pub fn is_collation_equivalent_to(&self, other: &Self) -> bool {
        self.binding == other.binding
            && self.collation_equivalence_bytes() == other.collation_equivalence_bytes()
    }

    /// Unique, bounded canonical value encoding for this text value.
    ///
    /// This is a scalar-value encoding, not a wire frame. All size arithmetic
    /// and allocation reservation are checked.
    pub fn encode(&self) -> Result<Vec<u8>, CanonicalTextError> {
        let binding_len = match self.binding {
            TextBinding::UcsBasic => 1,
            TextBinding::NonBinary(_) => 1 + 4 * OBJECT_ID_BYTES,
        };
        let key_len = self.sort_key.as_ref().map_or(0, |key| 8 + key.len());
        let capacity = 1usize
            .checked_add(binding_len)
            .and_then(|n| n.checked_add(8))
            .and_then(|n| n.checked_add(self.text.len()))
            .and_then(|n| n.checked_add(key_len))
            .ok_or(CanonicalTextError::EncodedSizeOverflow)?;
        let mut out = Vec::new();
        out.try_reserve_exact(capacity)
            .map_err(|_| CanonicalTextError::AllocationFailed {
                field: TextField::Encoding,
                requested: capacity,
            })?;
        out.push(TEXT_ENCODING_VERSION);
        match self.binding {
            TextBinding::UcsBasic => out.push(UCS_BASIC_TAG),
            TextBinding::NonBinary(binding) => {
                out.push(NON_BINARY_TAG);
                append_oid(&mut out, binding.unicode_data_oid);
                append_oid(&mut out, binding.normalization_oid);
                append_oid(&mut out, binding.segmentation_oid);
                append_oid(&mut out, binding.collation_oid);
            }
        }
        out.extend_from_slice(&(self.text.len() as u64).to_le_bytes());
        out.extend_from_slice(self.text.as_bytes());
        if let Some(key) = &self.sort_key {
            out.extend_from_slice(&(key.len() as u64).to_le_bytes());
            out.extend_from_slice(key);
        }
        Ok(out)
    }

    /// Decodes UCS_BASIC without any ambient collation capability.
    /// Non-binary values fail closed with [`CanonicalTextError::ResolverRequired`].
    pub fn decode(encoded: &[u8]) -> Result<Self, CanonicalTextError> {
        match parse_canonical_text(encoded)? {
            ParsedCanonicalText::UcsBasic { text } => Self::new_ucs_basic(text),
            ParsedCanonicalText::NonBinary { .. } => Err(CanonicalTextError::ResolverRequired),
        }
    }

    /// Decodes a canonical text value. For non-binary bindings the resolver
    /// recomputes the key and encoded bytes must match exactly.
    pub fn decode_with_resolver<R: CollationResolver + ?Sized>(
        encoded: &[u8],
        resolver: &R,
    ) -> Result<Self, CanonicalTextError> {
        match parse_canonical_text(encoded)? {
            ParsedCanonicalText::UcsBasic { text } => Self::new_ucs_basic(text),
            ParsedCanonicalText::NonBinary {
                text,
                binding,
                encoded_sort_key,
            } => {
                check_ordered_scalar_payload(text.len(), Some(encoded_sort_key.len()))?;
                binding.validate_artifacts(resolver)?;
                let matches = resolver
                    .canonical_sort_key_matches(&binding, text, encoded_sort_key)
                    .map_err(CanonicalTextError::ResolverFailure)?;
                if !matches {
                    return Err(CanonicalTextError::SortKeyMismatch {
                        encoded_len: encoded_sort_key.len(),
                        resolved_len: resolver
                            .canonical_sort_key_len(&binding, text)
                            .map_err(CanonicalTextError::ResolverFailure)?,
                    });
                }
                Self::from_resolved_non_binary(text, binding, copy_bytes(encoded_sort_key)?)
            }
        }
    }

    fn from_resolved_non_binary(
        text: &str,
        binding: NonBinaryTextBinding,
        sort_key: Vec<u8>,
    ) -> Result<Self, CanonicalTextError> {
        check_len(TextField::Text, text.len(), MAX_CANONICAL_TEXT_BYTES)?;
        check_len(
            TextField::SortKey,
            sort_key.len(),
            MAX_CANONICAL_SORT_KEY_BYTES,
        )?;
        check_ordered_scalar_payload(text.len(), Some(sort_key.len()))?;
        Ok(Self {
            text: copy_string(text)?,
            binding: TextBinding::NonBinary(binding),
            sort_key: Some(sort_key),
        })
    }
}

impl PartialOrd for CanonicalText {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CanonicalText {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.binding.cmp(&other.binding) {
            Ordering::Equal => {}
            order => return order,
        }
        match (&self.sort_key, &other.sort_key) {
            (None, None) => self.text.as_bytes().cmp(other.text.as_bytes()),
            (Some(left), Some(right)) => left
                .cmp(right)
                .then_with(|| self.text.as_bytes().cmp(other.text.as_bytes())),
            // Constructors and decode make these arms unreachable, but keep
            // ordering total even if future internal code violates the shape.
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
        }
    }
}

/// Role of an artifact in a non-binary text binding.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TextArtifactRole {
    UnicodeData,
    Normalization,
    Segmentation,
    Collation,
}

/// Field named by a typed bounds or malformed-input rejection.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TextField {
    Version,
    Binding,
    UnicodeDataObjectId,
    NormalizationObjectId,
    SegmentationObjectId,
    CollationObjectId,
    Text,
    SortKey,
    Encoding,
}

/// Fail-closed construction, encoding, and decoding errors.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CanonicalTextError {
    UnsupportedVersion(u8),
    UnknownBindingTag(u8),
    Truncated {
        field: TextField,
        needed: usize,
        remaining: usize,
    },
    TrailingBytes(usize),
    LengthOutOfRange {
        field: TextField,
        declared: u64,
        max: usize,
    },
    InvalidUtf8,
    MissingArtifact {
        role: TextArtifactRole,
        object_id: ObjectId,
    },
    ResolverRequired,
    ResolverFailure(CollationResolverError),
    ResolverOutputLengthMismatch {
        declared: usize,
        written: usize,
    },
    SortKeyMismatch {
        encoded_len: usize,
        resolved_len: usize,
    },
    OrderedScalarPayloadTooLarge {
        encoded: usize,
        maximum: usize,
    },
    EncodedSizeOverflow,
    AllocationFailed {
        field: TextField,
        requested: usize,
    },
}

impl std::fmt::Display for CanonicalTextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported canonical text version {version}")
            }
            Self::UnknownBindingTag(tag) => write!(f, "unknown text binding tag {tag:#04x}"),
            Self::Truncated {
                field,
                needed,
                remaining,
            } => write!(
                f,
                "truncated {field:?}: need {needed} bytes, have {remaining}"
            ),
            Self::TrailingBytes(count) => {
                write!(f, "canonical text has {count} trailing bytes")
            }
            Self::LengthOutOfRange {
                field,
                declared,
                max,
            } => write!(
                f,
                "{field:?} length {declared} exceeds canonical bound {max}"
            ),
            Self::InvalidUtf8 => write!(f, "canonical text payload is not UTF-8"),
            Self::MissingArtifact { role, object_id } => {
                write!(f, "missing {role:?} artifact {object_id:?}")
            }
            Self::ResolverRequired => {
                write!(f, "non-binary canonical text requires a collation resolver")
            }
            Self::ResolverFailure(error) => error.fmt(f),
            Self::ResolverOutputLengthMismatch { declared, written } => {
                write!(
                    f,
                    "collation resolver declared {declared} key bytes but wrote {written}"
                )
            }
            Self::SortKeyMismatch {
                encoded_len,
                resolved_len,
            } => {
                write!(
                    f,
                    "encoded canonical sort key ({encoded_len} bytes) does not match resolver output ({resolved_len} bytes)"
                )
            }
            Self::OrderedScalarPayloadTooLarge { encoded, maximum } => write!(
                f,
                "ordered scalar text payload length {encoded} exceeds aggregate bound {maximum}"
            ),
            Self::EncodedSizeOverflow => write!(f, "canonical text encoded size overflow"),
            Self::AllocationFailed { field, requested } => {
                write!(f, "unable to allocate {requested} bytes for {field:?}")
            }
        }
    }
}

impl std::error::Error for CanonicalTextError {}

fn check_len(field: TextField, len: usize, max: usize) -> Result<(), CanonicalTextError> {
    if len > max {
        return Err(CanonicalTextError::LengthOutOfRange {
            field,
            declared: len as u64,
            max,
        });
    }
    Ok(())
}

fn copy_string(value: &str) -> Result<String, CanonicalTextError> {
    let mut out = String::new();
    out.try_reserve_exact(value.len())
        .map_err(|_| CanonicalTextError::AllocationFailed {
            field: TextField::Text,
            requested: value.len(),
        })?;
    out.push_str(value);
    Ok(out)
}

fn copy_bytes(value: &[u8]) -> Result<Vec<u8>, CanonicalTextError> {
    let mut out = Vec::new();
    out.try_reserve_exact(value.len())
        .map_err(|_| CanonicalTextError::AllocationFailed {
            field: TextField::SortKey,
            requested: value.len(),
        })?;
    out.extend_from_slice(value);
    Ok(out)
}

fn comparable_encoded_len(decoded_len: usize) -> Option<usize> {
    decoded_len.checked_div(8)?.checked_add(1)?.checked_mul(9)
}

fn check_ordered_scalar_payload(
    text_len: usize,
    sort_key_len: Option<usize>,
) -> Result<(), CanonicalTextError> {
    let text_encoded =
        comparable_encoded_len(text_len).ok_or(CanonicalTextError::EncodedSizeOverflow)?;
    let encoded = match sort_key_len {
        None => 1usize
            .checked_add(text_encoded)
            .ok_or(CanonicalTextError::EncodedSizeOverflow)?,
        Some(sort_key_len) => 1usize
            .checked_add(4 * OBJECT_ID_BYTES)
            .and_then(|len| len.checked_add(comparable_encoded_len(sort_key_len)?))
            .and_then(|len| len.checked_add(text_encoded))
            .ok_or(CanonicalTextError::EncodedSizeOverflow)?,
    };
    if encoded > MAX_SCALAR_PAYLOAD {
        return Err(CanonicalTextError::OrderedScalarPayloadTooLarge {
            encoded,
            maximum: MAX_SCALAR_PAYLOAD,
        });
    }
    Ok(())
}

fn append_oid(output: &mut Vec<u8>, oid: ObjectId) {
    output.extend_from_slice(oid.as_bytes());
}

fn resolve_sort_key<R: CollationResolver + ?Sized>(
    binding: NonBinaryTextBinding,
    text: &str,
    resolver: &R,
) -> Result<Vec<u8>, CanonicalTextError> {
    binding.validate_artifacts(resolver)?;
    let declared = resolver
        .canonical_sort_key_len(&binding, text)
        .map_err(CanonicalTextError::ResolverFailure)?;
    check_len(TextField::SortKey, declared, MAX_CANONICAL_SORT_KEY_BYTES)?;
    check_ordered_scalar_payload(text.len(), Some(declared))?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(declared)
        .map_err(|_| CanonicalTextError::AllocationFailed {
            field: TextField::SortKey,
            requested: declared,
        })?;
    output.resize(declared, 0);
    let written = resolver
        .write_canonical_sort_key(&binding, text, &mut output)
        .map_err(CanonicalTextError::ResolverFailure)?;
    if written != declared {
        return Err(CanonicalTextError::ResolverOutputLengthMismatch { declared, written });
    }
    Ok(output)
}

enum ParsedCanonicalText<'a> {
    UcsBasic {
        text: &'a str,
    },
    NonBinary {
        text: &'a str,
        binding: NonBinaryTextBinding,
        encoded_sort_key: &'a [u8],
    },
}

fn parse_canonical_text(encoded: &[u8]) -> Result<ParsedCanonicalText<'_>, CanonicalTextError> {
    let mut decoder = Decoder::new(encoded);
    let version = decoder.read_u8(TextField::Version)?;
    if version != TEXT_ENCODING_VERSION {
        return Err(CanonicalTextError::UnsupportedVersion(version));
    }
    let tag = decoder.read_u8(TextField::Binding)?;
    let binding = match tag {
        UCS_BASIC_TAG => None,
        NON_BINARY_TAG => Some(NonBinaryTextBinding::new(
            decoder.read_oid(TextField::UnicodeDataObjectId)?,
            decoder.read_oid(TextField::NormalizationObjectId)?,
            decoder.read_oid(TextField::SegmentationObjectId)?,
            decoder.read_oid(TextField::CollationObjectId)?,
        )),
        other => return Err(CanonicalTextError::UnknownBindingTag(other)),
    };
    let text_len = decoder.read_bounded_len(TextField::Text, MAX_CANONICAL_TEXT_BYTES)?;
    let text_bytes = decoder.take(TextField::Text, text_len)?;
    let text = std::str::from_utf8(text_bytes).map_err(|_| CanonicalTextError::InvalidUtf8)?;

    match binding {
        None => {
            decoder.finish()?;
            Ok(ParsedCanonicalText::UcsBasic { text })
        }
        Some(binding) => {
            let key_len =
                decoder.read_bounded_len(TextField::SortKey, MAX_CANONICAL_SORT_KEY_BYTES)?;
            let encoded_sort_key = decoder.take(TextField::SortKey, key_len)?;
            decoder.finish()?;
            Ok(ParsedCanonicalText::NonBinary {
                text,
                binding,
                encoded_sort_key,
            })
        }
    }
}

struct Decoder<'a> {
    remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    const fn new(encoded: &'a [u8]) -> Self {
        Self { remaining: encoded }
    }

    fn take(&mut self, field: TextField, len: usize) -> Result<&'a [u8], CanonicalTextError> {
        if self.remaining.len() < len {
            return Err(CanonicalTextError::Truncated {
                field,
                needed: len,
                remaining: self.remaining.len(),
            });
        }
        let (head, tail) = self.remaining.split_at(len);
        self.remaining = tail;
        Ok(head)
    }

    fn read_u8(&mut self, field: TextField) -> Result<u8, CanonicalTextError> {
        let raw = self.take(field, 1)?;
        raw.first().copied().ok_or(CanonicalTextError::Truncated {
            field,
            needed: 1,
            remaining: 0,
        })
    }

    fn read_oid(&mut self, field: TextField) -> Result<ObjectId, CanonicalTextError> {
        let bytes = self.take(field, OBJECT_ID_BYTES)?;
        let raw: [u8; OBJECT_ID_BYTES] =
            bytes
                .try_into()
                .map_err(|_| CanonicalTextError::Truncated {
                    field,
                    needed: OBJECT_ID_BYTES,
                    remaining: bytes.len(),
                })?;
        Ok(ObjectId(raw))
    }

    fn read_bounded_len(
        &mut self,
        field: TextField,
        max: usize,
    ) -> Result<usize, CanonicalTextError> {
        let bytes = self.take(field, 8)?;
        let raw: [u8; 8] = bytes
            .try_into()
            .map_err(|_| CanonicalTextError::Truncated {
                field,
                needed: 8,
                remaining: bytes.len(),
            })?;
        let declared = u64::from_le_bytes(raw);
        let len = usize::try_from(declared).map_err(|_| CanonicalTextError::LengthOutOfRange {
            field,
            declared,
            max,
        })?;
        if len > max {
            return Err(CanonicalTextError::LengthOutOfRange {
                field,
                declared,
                max,
            });
        }
        Ok(len)
    }

    fn finish(self) -> Result<(), CanonicalTextError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(CanonicalTextError::TrailingBytes(self.remaining.len()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn oid(byte: u8) -> ObjectId {
        ObjectId([byte; OBJECT_ID_BYTES])
    }

    fn binding() -> NonBinaryTextBinding {
        NonBinaryTextBinding::new(oid(1), oid(2), oid(3), oid(4))
    }

    struct TestResolver {
        missing: Option<ObjectId>,
        constant_key: Option<u8>,
    }

    impl TestResolver {
        const fn available() -> Self {
            Self {
                missing: None,
                constant_key: None,
            }
        }

        const fn constant(key: u8) -> Self {
            Self {
                missing: None,
                constant_key: Some(key),
            }
        }
    }

    impl CollationResolver for TestResolver {
        fn artifact_available(&self, object_id: &ObjectId) -> bool {
            self.missing.as_ref() != Some(object_id)
        }

        fn canonical_sort_key_len(
            &self,
            _binding: &NonBinaryTextBinding,
            text: &str,
        ) -> Result<usize, CollationResolverError> {
            Ok(self.constant_key.map_or(text.len(), |_| 1))
        }

        fn write_canonical_sort_key(
            &self,
            _binding: &NonBinaryTextBinding,
            text: &str,
            output: &mut [u8],
        ) -> Result<usize, CollationResolverError> {
            if let Some(key) = self.constant_key {
                let Some(slot) = output.first_mut() else {
                    return Err(CollationResolverError::new(1));
                };
                *slot = key;
                return Ok(1);
            }
            if output.len() != text.len() {
                return Err(CollationResolverError::new(2));
            }
            for (slot, byte) in output.iter_mut().zip(text.bytes().rev()) {
                *slot = u8::MAX - byte;
            }
            Ok(output.len())
        }

        fn canonical_sort_key_matches(
            &self,
            _binding: &NonBinaryTextBinding,
            text: &str,
            candidate: &[u8],
        ) -> Result<bool, CollationResolverError> {
            if let Some(key) = self.constant_key {
                return Ok(candidate == [key]);
            }
            Ok(candidate.len() == text.len()
                && candidate
                    .iter()
                    .copied()
                    .zip(text.bytes().rev())
                    .all(|(actual, byte)| actual == u8::MAX - byte))
        }
    }

    fn hash(value: &CanonicalText) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    fn non_binary(text: &str, resolver: &impl CollationResolver) -> CanonicalText {
        CanonicalText::new_non_binary(text, binding(), resolver)
            .expect("test artifacts and resolver output are valid")
    }

    #[test]
    fn ucs_basic_is_binary_and_round_trips() {
        let a = CanonicalText::new_ucs_basic("Z").expect("small text must fit");
        let b = CanonicalText::new_ucs_basic("é").expect("small text must fit");
        assert!(a < b);
        assert_eq!(a.binding(), &TextBinding::UcsBasic);
        assert!(a.canonical_sort_key().is_none());
        let encoded = b.encode().expect("bounded encoding must allocate");
        assert_eq!(
            CanonicalText::decode(&encoded).expect("UCS_BASIC needs no resolver"),
            b
        );
    }

    #[test]
    fn non_binary_uses_key_then_utf8_tie_breaker() {
        let resolver = TestResolver::available();
        let lower = non_binary("z", &resolver);
        let higher = non_binary("a", &resolver);
        assert!(lower < higher, "sort key must dominate UTF-8 order");

        let tie_resolver = TestResolver::constant(7);
        let tie_a = non_binary("a", &tie_resolver);
        let tie_b = non_binary("b", &tie_resolver);
        assert!(
            tie_a < tie_b,
            "UTF-8 bytes deterministically break key ties"
        );
        assert_ne!(tie_a, tie_b);
        assert!(tie_a.is_collation_equivalent_to(&tie_b));
        assert_eq!(
            tie_a.collation_equivalence_bytes(),
            tie_b.collation_equivalence_bytes()
        );

        let binary_a = CanonicalText::new_ucs_basic("a").expect("small text must fit");
        let binary_b = CanonicalText::new_ucs_basic("b").expect("small text must fit");
        assert!(!binary_a.is_collation_equivalent_to(&binary_b));
        assert!(!binary_a.is_collation_equivalent_to(&tie_a));
    }

    #[test]
    fn equality_hash_order_and_encoding_are_coherent() {
        let resolver = TestResolver::available();
        let value = non_binary("Straße", &resolver);
        let encoded = value.encode().expect("bounded encoding must allocate");
        let decoded = CanonicalText::decode_with_resolver(&encoded, &resolver)
            .expect("valid pinned text must decode");
        assert_eq!(value, decoded);
        assert_eq!(value.cmp(&decoded), Ordering::Equal);
        assert_eq!(hash(&value), hash(&decoded));
        assert_eq!(
            encoded,
            decoded.encode().expect("re-encoding must allocate")
        );
    }

    #[test]
    fn every_missing_artifact_fails_closed() {
        let artifacts = binding();
        for (role, missing) in [
            (TextArtifactRole::UnicodeData, artifacts.unicode_data_oid),
            (TextArtifactRole::Normalization, artifacts.normalization_oid),
            (TextArtifactRole::Segmentation, artifacts.segmentation_oid),
            (TextArtifactRole::Collation, artifacts.collation_oid),
        ] {
            let resolver = TestResolver {
                missing: Some(missing),
                constant_key: Some(1),
            };
            let error = CanonicalText::new_non_binary("x", artifacts, &resolver)
                .expect_err("missing artifact must reject");
            assert_eq!(
                error,
                CanonicalTextError::MissingArtifact {
                    role,
                    object_id: missing
                }
            );
        }
    }

    #[test]
    fn non_binary_requires_resolver_and_rejects_forged_key() {
        let resolver = TestResolver::available();
        let value = non_binary("pinned", &resolver);
        let mut encoded = value.encode().expect("bounded encoding must allocate");
        assert_eq!(
            CanonicalText::decode(&encoded),
            Err(CanonicalTextError::ResolverRequired)
        );
        let Some(last) = encoded.last_mut() else {
            panic!("non-binary encoding must contain a key byte");
        };
        *last ^= 1;
        assert_eq!(
            CanonicalText::decode_with_resolver(&encoded, &resolver),
            Err(CanonicalTextError::SortKeyMismatch {
                encoded_len: "pinned".len(),
                resolved_len: "pinned".len(),
            })
        );
    }

    #[test]
    fn malformed_and_noncanonical_inputs_are_typed_rejections() {
        assert_eq!(
            CanonicalText::decode(&[]),
            Err(CanonicalTextError::Truncated {
                field: TextField::Version,
                needed: 1,
                remaining: 0
            })
        );
        assert_eq!(
            CanonicalText::decode(&[9, UCS_BASIC_TAG]),
            Err(CanonicalTextError::UnsupportedVersion(9))
        );
        assert_eq!(
            CanonicalText::decode(&[TEXT_ENCODING_VERSION, 9]),
            Err(CanonicalTextError::UnknownBindingTag(9))
        );

        let mut oversized = vec![TEXT_ENCODING_VERSION, UCS_BASIC_TAG];
        oversized.extend_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            CanonicalText::decode(&oversized),
            Err(CanonicalTextError::LengthOutOfRange {
                field: TextField::Text,
                declared: u64::MAX,
                max: MAX_CANONICAL_TEXT_BYTES
            })
        );

        let mut invalid_utf8 = vec![TEXT_ENCODING_VERSION, UCS_BASIC_TAG];
        invalid_utf8.extend_from_slice(&1u64.to_le_bytes());
        invalid_utf8.push(0xff);
        assert_eq!(
            CanonicalText::decode(&invalid_utf8),
            Err(CanonicalTextError::InvalidUtf8)
        );

        let mut trailing = CanonicalText::new_ucs_basic("ok")
            .expect("small text must fit")
            .encode()
            .expect("bounded encoding must allocate");
        trailing.push(0);
        assert_eq!(
            CanonicalText::decode(&trailing),
            Err(CanonicalTextError::TrailingBytes(1))
        );
    }

    #[test]
    fn decode_checks_artifacts_before_owned_payload_allocation() {
        let resolver = TestResolver::available();
        let value = non_binary("pinned", &resolver);
        let encoded = value.encode().expect("bounded encoding must allocate");
        let missing = binding().segmentation_oid;
        let missing_resolver = TestResolver {
            missing: Some(missing),
            constant_key: None,
        };
        assert_eq!(
            CanonicalText::decode_with_resolver(&encoded, &missing_resolver),
            Err(CanonicalTextError::MissingArtifact {
                role: TextArtifactRole::Segmentation,
                object_id: missing
            })
        );
    }

    struct FlippingResolver {
        next: Cell<u8>,
    }

    impl CollationResolver for FlippingResolver {
        fn artifact_available(&self, _: &ObjectId) -> bool {
            true
        }

        fn canonical_sort_key_len(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
        ) -> Result<usize, CollationResolverError> {
            Ok(1)
        }

        fn write_canonical_sort_key(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
            output: &mut [u8],
        ) -> Result<usize, CollationResolverError> {
            let Some(slot) = output.first_mut() else {
                return Err(CollationResolverError::new(3));
            };
            let value = self.next.get();
            *slot = value;
            self.next.set(value ^ 1);
            Ok(1)
        }

        fn canonical_sort_key_matches(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
            candidate: &[u8],
        ) -> Result<bool, CollationResolverError> {
            let value = self.next.get();
            self.next.set(value ^ 1);
            Ok(candidate == [value])
        }
    }

    #[test]
    fn decode_rejects_nondeterministic_resolver_output() {
        let resolver = FlippingResolver { next: Cell::new(0) };
        let value = CanonicalText::new_non_binary("x", binding(), &resolver)
            .expect("first resolution is bounded");
        let encoded = value.encode().expect("bounded encoding must allocate");
        assert_eq!(
            CanonicalText::decode_with_resolver(&encoded, &resolver),
            Err(CanonicalTextError::SortKeyMismatch {
                encoded_len: 1,
                resolved_len: 1,
            })
        );
    }

    struct BadLengthResolver {
        write_called: Cell<bool>,
        declared: usize,
        written: usize,
    }

    impl CollationResolver for BadLengthResolver {
        fn artifact_available(&self, _: &ObjectId) -> bool {
            true
        }

        fn canonical_sort_key_len(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
        ) -> Result<usize, CollationResolverError> {
            Ok(self.declared)
        }

        fn write_canonical_sort_key(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
            _: &mut [u8],
        ) -> Result<usize, CollationResolverError> {
            self.write_called.set(true);
            Ok(self.written)
        }

        fn canonical_sort_key_matches(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
            _: &[u8],
        ) -> Result<bool, CollationResolverError> {
            Ok(false)
        }
    }

    #[test]
    fn resolver_length_contract_and_preallocation_bound_are_enforced() {
        let mismatch = BadLengthResolver {
            write_called: Cell::new(false),
            declared: 2,
            written: 1,
        };
        assert_eq!(
            CanonicalText::new_non_binary("x", binding(), &mismatch),
            Err(CanonicalTextError::ResolverOutputLengthMismatch {
                declared: 2,
                written: 1,
            })
        );
        assert!(mismatch.write_called.get());

        let oversized = BadLengthResolver {
            write_called: Cell::new(false),
            declared: MAX_CANONICAL_SORT_KEY_BYTES + 1,
            written: 0,
        };
        assert_eq!(
            CanonicalText::new_non_binary("x", binding(), &oversized),
            Err(CanonicalTextError::LengthOutOfRange {
                field: TextField::SortKey,
                declared: (MAX_CANONICAL_SORT_KEY_BYTES + 1) as u64,
                max: MAX_CANONICAL_SORT_KEY_BYTES,
            })
        );
        assert!(!oversized.write_called.get());
    }

    #[test]
    fn aggregate_scalar_bound_is_checked_before_key_allocation() {
        let max_ucs_groups = (MAX_SCALAR_PAYLOAD - 1) / 9;
        let max_ucs_text = max_ucs_groups * 8 - 1;
        assert!(check_ordered_scalar_payload(max_ucs_text, None).is_ok());
        assert!(matches!(
            check_ordered_scalar_payload(max_ucs_text + 1, None),
            Err(CanonicalTextError::OrderedScalarPayloadTooLarge { .. })
        ));

        let aggregate_oversized = BadLengthResolver {
            write_called: Cell::new(false),
            declared: MAX_CANONICAL_SORT_KEY_BYTES,
            written: 0,
        };
        assert!(matches!(
            CanonicalText::new_non_binary("x", binding(), &aggregate_oversized),
            Err(CanonicalTextError::OrderedScalarPayloadTooLarge { .. })
        ));
        assert!(!aggregate_oversized.write_called.get());

        let forged_small_key = BadLengthResolver {
            write_called: Cell::new(false),
            declared: MAX_CANONICAL_SORT_KEY_BYTES,
            written: 0,
        };
        assert_eq!(
            CanonicalText::from_ordered_scalar_parts(
                String::from("x"),
                binding(),
                vec![0],
                &forged_small_key,
            ),
            Err(CanonicalTextError::SortKeyMismatch {
                encoded_len: 1,
                resolved_len: MAX_CANONICAL_SORT_KEY_BYTES,
            })
        );
        assert!(!forged_small_key.write_called.get());
    }
}
