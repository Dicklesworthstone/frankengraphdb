//! Profile-bound canonical property values.
//!
//! CanonicalScalar remains the closed atomic value domain. Stored graph
//! properties use CanonicalPropertyValue so recursive List and Map values do
//! not blur the scalar/query-type boundary from plan section 4.1. Every
//! operation that admits durable/result bytes is routed through a verified
//! CanonicalScalarProfile; no host locale or timezone state is consulted.

use core::{cmp::Ordering, hash::Hasher};

use crate::{
    decimal::{
        CanonicalDecimal, DecimalError, STRICT_PORTABLE_DECIMAL_PRECISION,
        STRICT_PORTABLE_DECIMAL_SCALE, STRICT_PORTABLE_MAX_DECIMAL_SCALE,
    },
    ids::ObjectId,
    scalar::{
        CanonicalScalar, CanonicalScalarResolver, MAX_SCALAR_PAYLOAD,
        STRICT_PORTABLE_SCALAR_DESCRIPTOR_LEN, ScalarDecodeError, ScalarEncodeError,
        append_strict_portable_scalar_descriptor, canonical_text_scalar_encoded_len,
        encode_canonical_text_into,
    },
    temporal::TimestampArtifactError,
    text::{CanonicalText, CanonicalTextError, TextBinding},
};

const PROFILE_DESCRIPTOR_DOMAIN: &[u8] = b"fgdb:canonical-scalar-profile:strict-portable:v1\0";
const PROFILE_DESCRIPTOR_VERSION: u16 = 1;
const PROFILE_KIND_STRICT_PORTABLE: u8 = 1;
const FLOAT_POLICY_CANONICAL_NAN_AND_POSITIVE_ZERO: u8 = 1;
const DECIMAL_ROUNDING_HALF_EVEN: u8 = 1;
const OVERFLOW_POLICY_REJECT: u8 = 1;
const MAP_KEY_ORDER_CANONICAL_SCALAR_BYTES: u8 = 1;
const COERCION_POLICY_EXACT_NUMERIC_ONLY: u8 = 1;
const HASH_FEED_SINGLE_WRITE: u8 = 1;
const HASH_FEED_DOMAIN_THEN_CANONICAL_PROPERTY_BYTES: u8 = 1;
const NULL_COERCION_IDENTITY_ONLY: u8 = 1;
const IDENTITY_COERCION_KIND_MASK: u8 = 0xff;
const CROSS_KIND_COERCION_COUNT: u8 = 2;
const PROPERTY_HASH_DOMAIN: &[u8] = b"fgdb:canonical-property-value-hash:v1\0";

const TAG_LIST: u8 = 0x08;
const TAG_MAP: u8 = 0x09;
const COLLECTION_END: u8 = 0x00;
const COLLECTION_ITEM: u8 = 0x01;
const ORDERED_FIELD_END: u8 = 0x00;
const ORDERED_FIELD_BYTE: u8 = 0x01;
const PROFILE_FIXED_RULE_BYTES: &[u8] = &[
    PROFILE_KIND_STRICT_PORTABLE,
    FLOAT_POLICY_CANONICAL_NAN_AND_POSITIVE_ZERO,
    DECIMAL_ROUNDING_HALF_EVEN,
    OVERFLOW_POLICY_REJECT,
    MAP_KEY_ORDER_CANONICAL_SCALAR_BYTES,
    COERCION_POLICY_EXACT_NUMERIC_ONLY,
    HASH_FEED_SINGLE_WRITE,
    HASH_FEED_DOMAIN_THEN_CANONICAL_PROPERTY_BYTES,
    NULL_COERCION_IDENTITY_ONLY,
    IDENTITY_COERCION_KIND_MASK,
    CROSS_KIND_COERCION_COUNT,
    CanonicalScalarKind::Int as u8,
    CanonicalScalarKind::Decimal as u8,
    CanonicalScalarKind::Decimal as u8,
    CanonicalScalarKind::Int as u8,
    TAG_LIST,
    TAG_MAP,
    COLLECTION_END,
    COLLECTION_ITEM,
    ORDERED_FIELD_END,
    ORDERED_FIELD_BYTE,
];

/// Maximum non-binary collation artifacts bound by one scalar profile.
pub const MAX_PROFILE_COLLATIONS: usize = 1_024;

/// Maximum recursive List/Map nesting admitted by a property value.
pub const MAX_PROPERTY_NESTING_DEPTH: usize = 64;

/// Maximum number of direct children in one List or Map.
pub const MAX_PROPERTY_COLLECTION_ENTRIES: usize = 1_048_576;

/// Maximum complete canonical property-value encoding.
pub const MAX_PROPERTY_VALUE_BYTES: usize = MAX_SCALAR_PAYLOAD;

const DECIMAL_INTEGER_UNIT: i128 = 1_000_000_000_000_000_000;

/// Authority that binds a claimed profile ObjectId to the complete canonical
/// profile descriptor. Implementations may use the repository's content
/// address authority, but this type layer never guesses an identity scheme.
pub trait CanonicalScalarProfileIdentityVerifier {
    fn verify_canonical_scalar_profile_oid(
        &self,
        claimed_oid: ObjectId,
        canonical_profile: &[u8],
    ) -> bool;
}

/// Artifact role named by a profile-construction or value-admission failure.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ScalarProfileArtifactRole {
    UnicodeData,
    Normalization,
    Segmentation,
    Collation,
    Tzdb,
}

/// First-class immutable STRICT_PORTABLE scalar profile.
///
/// The private fields can only be populated after an identity authority binds
/// profile_oid to every rule and artifact ObjectId in canonical_descriptor.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CanonicalScalarProfile {
    profile_oid: ObjectId,
    unicode_data_oid: ObjectId,
    normalization_oid: ObjectId,
    segmentation_oid: ObjectId,
    tzdb_oid: ObjectId,
    non_binary_collation_oids: Box<[ObjectId]>,
}

impl CanonicalScalarProfile {
    /// Returns the canonical descriptor bytes an identity authority must bind.
    ///
    /// Collation input order is not semantic: the descriptor sorts the set and
    /// rejects duplicates. Every fixed STRICT_PORTABLE rule is named in the
    /// descriptor, so changing a listed bound, collection tag/framing byte,
    /// rounding rule, or coercion policy necessarily changes the verified
    /// identity input.
    pub fn try_canonical_descriptor_bytes(
        unicode_data_oid: ObjectId,
        normalization_oid: ObjectId,
        segmentation_oid: ObjectId,
        tzdb_oid: ObjectId,
        non_binary_collation_oids: &[ObjectId],
    ) -> Result<Vec<u8>, CanonicalScalarProfileError> {
        let collations = canonical_collation_set(non_binary_collation_oids)?;
        descriptor_from_canonical_collations(
            unicode_data_oid,
            normalization_oid,
            segmentation_oid,
            tzdb_oid,
            &collations,
        )
    }

    /// Constructs a usable profile only after identity and artifact checks.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new_verified<V, R>(
        profile_oid: ObjectId,
        unicode_data_oid: ObjectId,
        normalization_oid: ObjectId,
        segmentation_oid: ObjectId,
        tzdb_oid: ObjectId,
        non_binary_collation_oids: &[ObjectId],
        verifier: &V,
        resolver: &R,
    ) -> Result<Self, CanonicalScalarProfileError>
    where
        V: CanonicalScalarProfileIdentityVerifier + ?Sized,
        R: CanonicalScalarResolver,
    {
        let collations = canonical_collation_set(non_binary_collation_oids)?;
        let descriptor = descriptor_from_canonical_collations(
            unicode_data_oid,
            normalization_oid,
            segmentation_oid,
            tzdb_oid,
            &collations,
        )?;
        if !verifier.verify_canonical_scalar_profile_oid(profile_oid, &descriptor) {
            return Err(CanonicalScalarProfileError::ProfileIdentityUnverified {
                claimed: profile_oid,
            });
        }
        let profile = Self {
            profile_oid,
            unicode_data_oid,
            normalization_oid,
            segmentation_oid,
            tzdb_oid,
            non_binary_collation_oids: collations.into_boxed_slice(),
        };
        profile.validate_profile_artifacts(resolver)?;
        Ok(profile)
    }

    #[must_use]
    pub const fn profile_oid(&self) -> ObjectId {
        self.profile_oid
    }

    #[must_use]
    pub const fn unicode_data_oid(&self) -> ObjectId {
        self.unicode_data_oid
    }

    #[must_use]
    pub const fn normalization_oid(&self) -> ObjectId {
        self.normalization_oid
    }

    #[must_use]
    pub const fn segmentation_oid(&self) -> ObjectId {
        self.segmentation_oid
    }

    #[must_use]
    pub const fn tzdb_oid(&self) -> ObjectId {
        self.tzdb_oid
    }

    #[must_use]
    pub fn non_binary_collation_oids(&self) -> &[ObjectId] {
        &self.non_binary_collation_oids
    }

    /// Revalidates that every artifact named by the profile is available from
    /// the exact resolver. There is no host fallback.
    pub fn validate_profile_artifacts<R: CanonicalScalarResolver>(
        &self,
        resolver: &R,
    ) -> Result<(), CanonicalScalarProfileError> {
        for (role, oid) in [
            (
                ScalarProfileArtifactRole::UnicodeData,
                self.unicode_data_oid,
            ),
            (
                ScalarProfileArtifactRole::Normalization,
                self.normalization_oid,
            ),
            (
                ScalarProfileArtifactRole::Segmentation,
                self.segmentation_oid,
            ),
        ] {
            if !resolver.artifact_available(&oid) {
                return Err(CanonicalScalarProfileError::MissingArtifact {
                    role,
                    object_id: oid,
                });
            }
        }
        for &oid in &self.non_binary_collation_oids {
            if !resolver.artifact_available(&oid) {
                return Err(CanonicalScalarProfileError::MissingArtifact {
                    role: ScalarProfileArtifactRole::Collation,
                    object_id: oid,
                });
            }
        }
        if !resolver.contains_tzdb(&self.tzdb_oid) {
            return Err(CanonicalScalarProfileError::MissingArtifact {
                role: ScalarProfileArtifactRole::Tzdb,
                object_id: self.tzdb_oid,
            });
        }
        Ok(())
    }

    /// Validates one recursive property value against this exact profile.
    pub fn validate_value<R: CanonicalScalarResolver>(
        &self,
        value: &CanonicalPropertyValue,
        resolver: &R,
    ) -> Result<(), CanonicalScalarProfileError> {
        self.validate_profile_artifacts(resolver)?;
        self.validate_value_bindings(value, resolver)?;
        value
            .canonical_encoded_size()
            .map(|_| ())
            .map_err(CanonicalScalarProfileError::PropertyValue)
    }

    /// Emits canonical order-preserving property bytes after profile admission.
    pub fn encode_value<R: CanonicalScalarResolver>(
        &self,
        value: &CanonicalPropertyValue,
        resolver: &R,
    ) -> Result<Vec<u8>, CanonicalScalarProfileError> {
        self.validate_profile_artifacts(resolver)?;
        self.validate_value_bindings(value, resolver)?;
        value
            .encode_canonical()
            .map_err(CanonicalScalarProfileError::PropertyValue)
    }

    /// Decodes and admits one complete property value under this profile.
    ///
    /// Scalar decoding, unique ordered-field framing, and monotonic Map-key
    /// validation reject every noncanonical form while parsing; decoding never
    /// sorts, repairs, or re-encodes hostile input.
    pub fn decode_value_with_resolver<R: CanonicalScalarResolver>(
        &self,
        bytes: &[u8],
        resolver: &R,
    ) -> Result<CanonicalPropertyValue, CanonicalScalarProfileError> {
        self.validate_profile_artifacts(resolver)?;
        let value = CanonicalPropertyValue::decode_canonical(bytes, resolver)
            .map_err(CanonicalScalarProfileError::PropertyValue)?;
        self.validate_value_bindings(&value, resolver)?;
        Ok(value)
    }

    /// Compares two admitted values under the profile's total order.
    pub fn compare<R: CanonicalScalarResolver>(
        &self,
        left: &CanonicalPropertyValue,
        right: &CanonicalPropertyValue,
        resolver: &R,
    ) -> Result<Ordering, CanonicalScalarProfileError> {
        self.validate_value(left, resolver)?;
        self.validate_value(right, resolver)?;
        Ok(left.cmp(right))
    }

    /// Hashes one admitted value using the caller's declared hash algorithm.
    ///
    /// The profile governs admission and value semantics; the caller remains
    /// responsible for pinning the concrete `Hasher` wherever hash bytes or
    /// bucket placement are durable or replay-visible. The complete domain
    /// separator followed by canonical value bytes is passed in exactly one
    /// `Hasher::write` call; that call boundary is part of the profile.
    pub fn hash_value<R, H>(
        &self,
        value: &CanonicalPropertyValue,
        resolver: &R,
        state: &mut H,
    ) -> Result<(), CanonicalScalarProfileError>
    where
        R: CanonicalScalarResolver,
        H: Hasher,
    {
        self.validate_profile_artifacts(resolver)?;
        self.validate_value_bindings(value, resolver)?;
        let feed = value
            .encode_canonical_with_prefix(PROPERTY_HASH_DOMAIN)
            .map_err(CanonicalScalarProfileError::PropertyValue)?;
        state.write(&feed);
        Ok(())
    }

    /// Applies the profile's exact-only numeric storage coercion.
    ///
    /// This is deliberately not the future LanguageContract query coercion
    /// lattice. Identity is total, including Null -> Null; the only cross-kind
    /// rules are exact Int/Decimal conversions. Untyped storage Null cannot
    /// impersonate a requested target kind. Float conversion is deferred to
    /// the pinned execution-kernel contract. The input is consumed so an
    /// identity coercion preserves large Text/Bytes allocations without an
    /// infallible clone hidden inside this `Result` API.
    pub fn coerce_scalar<R: CanonicalScalarResolver>(
        &self,
        value: CanonicalScalar,
        target: CanonicalScalarKind,
        resolver: &R,
    ) -> Result<CanonicalScalar, CanonicalScalarCoercionError> {
        self.validate_profile_artifacts(resolver)
            .map_err(CanonicalScalarCoercionError::Profile)?;
        self.validate_scalar_binding(&value, resolver)
            .map_err(CanonicalScalarCoercionError::Profile)?;

        let source = CanonicalScalarKind::of(&value);
        if source == target {
            return Ok(value);
        }
        match (value, target) {
            (CanonicalScalar::Int(integer), CanonicalScalarKind::Decimal) => {
                CanonicalDecimal::from_integer(i128::from(integer))
                    .map(CanonicalScalar::Decimal)
                    .map_err(CanonicalScalarCoercionError::Decimal)
            }
            (CanonicalScalar::Decimal(decimal), CanonicalScalarKind::Int) => {
                let coefficient = decimal.coefficient();
                if coefficient % DECIMAL_INTEGER_UNIT != 0 {
                    return Err(CanonicalScalarCoercionError::InexactNumeric { source, target });
                }
                let integer = coefficient / DECIMAL_INTEGER_UNIT;
                i64::try_from(integer)
                    .map(CanonicalScalar::Int)
                    .map_err(|_| CanonicalScalarCoercionError::InexactNumeric { source, target })
            }
            _ => Err(CanonicalScalarCoercionError::Unsupported { source, target }),
        }
    }

    fn validate_value_bindings<R: CanonicalScalarResolver>(
        &self,
        value: &CanonicalPropertyValue,
        resolver: &R,
    ) -> Result<(), CanonicalScalarProfileError> {
        match value {
            CanonicalPropertyValue::Scalar(scalar) => {
                self.validate_scalar_binding(scalar, resolver)
            }
            CanonicalPropertyValue::List(list) => {
                for value in list.values() {
                    self.validate_value_bindings(value, resolver)?;
                }
                Ok(())
            }
            CanonicalPropertyValue::Map(map) => {
                for entry in map.entries() {
                    self.validate_text_binding(entry.key(), resolver)?;
                    self.validate_value_bindings(entry.value(), resolver)?;
                }
                Ok(())
            }
        }
    }

    fn validate_scalar_binding<R: CanonicalScalarResolver>(
        &self,
        scalar: &CanonicalScalar,
        resolver: &R,
    ) -> Result<(), CanonicalScalarProfileError> {
        match scalar {
            CanonicalScalar::Text(text) => self.validate_text_binding(text, resolver),
            CanonicalScalar::Timestamp(timestamp) => {
                if let Some(zone) = timestamp.zone() {
                    if zone.tzdb_oid() != self.tzdb_oid {
                        return Err(CanonicalScalarProfileError::ArtifactBindingMismatch {
                            role: ScalarProfileArtifactRole::Tzdb,
                            expected: self.tzdb_oid,
                            actual: zone.tzdb_oid(),
                        });
                    }
                    timestamp
                        .validate_tzdb_binding(resolver)
                        .map_err(CanonicalScalarProfileError::Timestamp)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn validate_text_binding<R: CanonicalScalarResolver>(
        &self,
        text: &CanonicalText,
        resolver: &R,
    ) -> Result<(), CanonicalScalarProfileError> {
        if let TextBinding::NonBinary(binding) = *text.binding() {
            for (role, expected, actual) in [
                (
                    ScalarProfileArtifactRole::UnicodeData,
                    self.unicode_data_oid,
                    binding.unicode_data_oid,
                ),
                (
                    ScalarProfileArtifactRole::Normalization,
                    self.normalization_oid,
                    binding.normalization_oid,
                ),
                (
                    ScalarProfileArtifactRole::Segmentation,
                    self.segmentation_oid,
                    binding.segmentation_oid,
                ),
            ] {
                if expected != actual {
                    return Err(CanonicalScalarProfileError::ArtifactBindingMismatch {
                        role,
                        expected,
                        actual,
                    });
                }
            }
            if self
                .non_binary_collation_oids
                .binary_search(&binding.collation_oid)
                .is_err()
            {
                return Err(CanonicalScalarProfileError::CollationNotAdmitted {
                    actual: binding.collation_oid,
                });
            }
            binding
                .validate_artifacts(resolver)
                .map_err(CanonicalScalarProfileError::Text)?;
        }
        Ok(())
    }
}

fn canonical_collation_set(
    collations: &[ObjectId],
) -> Result<Vec<ObjectId>, CanonicalScalarProfileError> {
    if collations.len() > MAX_PROFILE_COLLATIONS {
        return Err(CanonicalScalarProfileError::TooManyCollations {
            actual: collations.len(),
            maximum: MAX_PROFILE_COLLATIONS,
        });
    }
    let mut canonical = Vec::new();
    canonical.try_reserve_exact(collations.len()).map_err(|_| {
        CanonicalScalarProfileError::AllocationFailed {
            requested: collations.len(),
        }
    })?;
    canonical.extend_from_slice(collations);
    canonical.sort_unstable();
    if let Some(duplicate) = canonical
        .windows(2)
        .find_map(|pair| (pair[0] == pair[1]).then_some(pair[0]))
    {
        return Err(CanonicalScalarProfileError::DuplicateCollation {
            object_id: duplicate,
        });
    }
    Ok(canonical)
}

fn descriptor_from_canonical_collations(
    unicode_data_oid: ObjectId,
    normalization_oid: ObjectId,
    segmentation_oid: ObjectId,
    tzdb_oid: ObjectId,
    collations: &[ObjectId],
) -> Result<Vec<u8>, CanonicalScalarProfileError> {
    let collation_bytes = collations
        .len()
        .checked_mul(32)
        .ok_or(CanonicalScalarProfileError::DescriptorSizeOverflow)?;
    let requested = PROFILE_DESCRIPTOR_DOMAIN
        .len()
        .checked_add(
            2 + PROFILE_FIXED_RULE_BYTES.len()
                + STRICT_PORTABLE_SCALAR_DESCRIPTOR_LEN
                + PROPERTY_HASH_DOMAIN.len()
                + 3 * 4
                + 5 * 8
                + 4 * 32
                + 4,
        )
        .and_then(|size| size.checked_add(collation_bytes))
        .ok_or(CanonicalScalarProfileError::DescriptorSizeOverflow)?;
    let mut out = Vec::new();
    out.try_reserve_exact(requested)
        .map_err(|_| CanonicalScalarProfileError::AllocationFailed { requested })?;
    out.extend_from_slice(PROFILE_DESCRIPTOR_DOMAIN);
    out.extend_from_slice(&PROFILE_DESCRIPTOR_VERSION.to_le_bytes());
    out.extend_from_slice(PROFILE_FIXED_RULE_BYTES);
    append_strict_portable_scalar_descriptor(&mut out);
    out.extend_from_slice(PROPERTY_HASH_DOMAIN);
    out.extend_from_slice(&STRICT_PORTABLE_DECIMAL_PRECISION.to_le_bytes());
    out.extend_from_slice(&STRICT_PORTABLE_DECIMAL_SCALE.to_le_bytes());
    out.extend_from_slice(&STRICT_PORTABLE_MAX_DECIMAL_SCALE.to_le_bytes());
    for value in [
        MAX_SCALAR_PAYLOAD,
        MAX_PROPERTY_VALUE_BYTES,
        MAX_PROPERTY_NESTING_DEPTH,
        MAX_PROPERTY_COLLECTION_ENTRIES,
        MAX_PROFILE_COLLATIONS,
    ] {
        let value = u64::try_from(value)
            .map_err(|_| CanonicalScalarProfileError::DescriptorSizeOverflow)?;
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(unicode_data_oid.as_bytes());
    out.extend_from_slice(normalization_oid.as_bytes());
    out.extend_from_slice(segmentation_oid.as_bytes());
    out.extend_from_slice(tzdb_oid.as_bytes());
    let count = u32::try_from(collations.len())
        .map_err(|_| CanonicalScalarProfileError::DescriptorSizeOverflow)?;
    out.extend_from_slice(&count.to_le_bytes());
    for oid in collations {
        out.extend_from_slice(oid.as_bytes());
    }
    debug_assert_eq!(out.len(), requested);
    Ok(out)
}

/// Closed atomic target vocabulary for profile coercion.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum CanonicalScalarKind {
    Null = 0,
    Bool = 1,
    Int = 2,
    Decimal = 3,
    Float = 4,
    Text = 5,
    Timestamp = 6,
    Bytes = 7,
}

impl CanonicalScalarKind {
    #[must_use]
    pub const fn of(value: &CanonicalScalar) -> Self {
        match value {
            CanonicalScalar::Null => Self::Null,
            CanonicalScalar::Bool(_) => Self::Bool,
            CanonicalScalar::Int(_) => Self::Int,
            CanonicalScalar::Decimal(_) => Self::Decimal,
            CanonicalScalar::Float(_) => Self::Float,
            CanonicalScalar::Text(_) => Self::Text,
            CanonicalScalar::Timestamp(_) => Self::Timestamp,
            CanonicalScalar::Bytes(_) => Self::Bytes,
        }
    }
}

/// Recursive stored-property value domain from plan section 4.1.
///
/// Its ordinary `Hash` implementation is equality-coherent for in-process
/// collections. Durable or replay-visible hashing must use
/// [`CanonicalScalarProfile::hash_value`], whose feed is the versioned,
/// domain-separated canonical byte encoding.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum CanonicalPropertyValue {
    Scalar(CanonicalScalar),
    List(CanonicalList),
    Map(CanonicalMap),
}

impl From<CanonicalScalar> for CanonicalPropertyValue {
    fn from(value: CanonicalScalar) -> Self {
        Self::Scalar(value)
    }
}

impl CanonicalPropertyValue {
    fn nesting_depth(&self) -> usize {
        match self {
            Self::Scalar(_) => 0,
            Self::List(list) => list.nesting_depth,
            Self::Map(map) => map.nesting_depth,
        }
    }

    fn canonical_encoded_size(&self) -> Result<usize, CanonicalPropertyValueError> {
        let encoded_size = match self {
            Self::Scalar(scalar) => scalar
                .canonical_encoded_len()
                .map_err(CanonicalPropertyValueError::ScalarEncode)?,
            Self::List(list) => list.encoded_size,
            Self::Map(map) => map.encoded_size,
        };
        check_encoded_size(encoded_size)?;
        Ok(encoded_size)
    }

    fn encode_canonical(&self) -> Result<Vec<u8>, CanonicalPropertyValueError> {
        self.encode_canonical_with_prefix(&[])
    }

    fn encode_canonical_with_prefix(
        &self,
        prefix: &[u8],
    ) -> Result<Vec<u8>, CanonicalPropertyValueError> {
        let encoded_size = self.canonical_encoded_size()?;
        let mut writer = PropertyValueWriter::with_prefix(encoded_size, prefix)?;
        writer.value(self, 0, 0)?;
        writer.finish()
    }

    fn decode_canonical<R: CanonicalScalarResolver>(
        bytes: &[u8],
        resolver: &R,
    ) -> Result<Self, CanonicalPropertyValueError> {
        if bytes.len() > MAX_PROPERTY_VALUE_BYTES {
            return Err(CanonicalPropertyValueError::EncodedValueTooLarge {
                actual: bytes.len(),
                maximum: MAX_PROPERTY_VALUE_BYTES,
            });
        }
        decode_property_value(bytes, resolver, 0)
    }
}

/// Canonical recursive List. Element order is semantic and preserved.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CanonicalList {
    values: Vec<CanonicalPropertyValue>,
    nesting_depth: usize,
    encoded_size: usize,
}

impl CanonicalList {
    pub fn try_new(
        values: Vec<CanonicalPropertyValue>,
    ) -> Result<Self, CanonicalPropertyValueError> {
        check_collection_count(values.len())?;
        let nesting_depth =
            collection_nesting_depth(values.iter().map(CanonicalPropertyValue::nesting_depth))?;
        check_nesting_depth(nesting_depth)?;
        let encoded_size = list_encoded_size(&values)?;
        Ok(Self {
            values,
            nesting_depth,
            encoded_size,
        })
    }

    fn from_canonical_decoded(
        values: Vec<CanonicalPropertyValue>,
        encoded_size: usize,
    ) -> Result<Self, CanonicalPropertyValueError> {
        check_collection_count(values.len())?;
        let nesting_depth =
            collection_nesting_depth(values.iter().map(CanonicalPropertyValue::nesting_depth))?;
        check_nesting_depth(nesting_depth)?;
        check_encoded_size(encoded_size)?;
        Ok(Self {
            values,
            nesting_depth,
            encoded_size,
        })
    }

    #[must_use]
    pub fn values(&self) -> &[CanonicalPropertyValue] {
        &self.values
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// One String-keyed Map entry. Map construction owns canonical ordering.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CanonicalMapEntry {
    key: CanonicalText,
    value: CanonicalPropertyValue,
}

impl CanonicalMapEntry {
    #[must_use]
    pub fn new(key: CanonicalText, value: CanonicalPropertyValue) -> Self {
        Self { key, value }
    }

    #[must_use]
    pub fn key(&self) -> &CanonicalText {
        &self.key
    }

    #[must_use]
    pub fn value(&self) -> &CanonicalPropertyValue {
        &self.value
    }
}

/// Canonical String-keyed Map sorted by each key's ordered scalar bytes.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CanonicalMap {
    entries: Vec<CanonicalMapEntry>,
    nesting_depth: usize,
    encoded_size: usize,
}

impl CanonicalMap {
    pub fn try_new(entries: Vec<CanonicalMapEntry>) -> Result<Self, CanonicalPropertyValueError> {
        check_collection_count(entries.len())?;
        let nesting_depth =
            collection_nesting_depth(entries.iter().map(|entry| entry.value.nesting_depth()))?;
        check_nesting_depth(nesting_depth)?;

        let mut encoded_size = 2usize; // tag + collection terminator
        for entry in &entries {
            let key_len = canonical_text_scalar_encoded_len(entry.key())
                .map_err(CanonicalPropertyValueError::ScalarEncode)?;
            let key_size = ordered_field_encoded_size(key_len)?;
            let value_size = ordered_field_encoded_size(entry.value().canonical_encoded_size()?)?;
            encoded_size = checked_encoded_add(encoded_size, 1)?; // item control
            encoded_size = checked_encoded_add(encoded_size, key_size)?;
            encoded_size = checked_encoded_add(encoded_size, value_size)?;
        }
        let mut entries = entries;
        entries.sort_unstable_by(|left, right| left.key().cmp(right.key()));
        if entries
            .windows(2)
            .any(|pair| pair[0].key() == pair[1].key())
        {
            return Err(CanonicalPropertyValueError::DuplicateMapKey);
        }
        Ok(Self {
            entries,
            nesting_depth,
            encoded_size,
        })
    }

    fn from_canonical_decoded(
        entries: Vec<CanonicalMapEntry>,
        encoded_size: usize,
    ) -> Result<Self, CanonicalPropertyValueError> {
        check_collection_count(entries.len())?;
        let nesting_depth =
            collection_nesting_depth(entries.iter().map(|entry| entry.value.nesting_depth()))?;
        check_nesting_depth(nesting_depth)?;
        check_encoded_size(encoded_size)?;
        Ok(Self {
            entries,
            nesting_depth,
            encoded_size,
        })
    }

    #[must_use]
    pub fn entries(&self) -> &[CanonicalMapEntry] {
        &self.entries
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn collection_nesting_depth(
    depths: impl Iterator<Item = usize>,
) -> Result<usize, CanonicalPropertyValueError> {
    1usize.checked_add(depths.max().unwrap_or(0)).ok_or(
        CanonicalPropertyValueError::NestingDepthExceeded {
            actual: usize::MAX,
            maximum: MAX_PROPERTY_NESTING_DEPTH,
        },
    )
}

fn list_encoded_size(
    values: &[CanonicalPropertyValue],
) -> Result<usize, CanonicalPropertyValueError> {
    let mut encoded_size = 2usize; // tag + collection terminator
    for value in values {
        let child_size = value.canonical_encoded_size()?;
        let field_size = ordered_field_encoded_size(child_size)?;
        encoded_size = checked_encoded_add(encoded_size, 1)?; // item control
        encoded_size = checked_encoded_add(encoded_size, field_size)?;
    }
    Ok(encoded_size)
}

fn ordered_field_encoded_size(decoded_size: usize) -> Result<usize, CanonicalPropertyValueError> {
    decoded_size
        .checked_mul(2)
        .and_then(|size| size.checked_add(1))
        .ok_or(CanonicalPropertyValueError::EncodedSizeOverflow)
}

fn checked_encoded_add(
    current: usize,
    additional: usize,
) -> Result<usize, CanonicalPropertyValueError> {
    let actual = current
        .checked_add(additional)
        .ok_or(CanonicalPropertyValueError::EncodedSizeOverflow)?;
    check_encoded_size(actual)?;
    Ok(actual)
}

fn check_encoded_size(actual: usize) -> Result<(), CanonicalPropertyValueError> {
    if actual > MAX_PROPERTY_VALUE_BYTES {
        return Err(CanonicalPropertyValueError::EncodedValueTooLarge {
            actual,
            maximum: MAX_PROPERTY_VALUE_BYTES,
        });
    }
    Ok(())
}

fn check_collection_count(actual: usize) -> Result<(), CanonicalPropertyValueError> {
    if actual > MAX_PROPERTY_COLLECTION_ENTRIES {
        return Err(CanonicalPropertyValueError::TooManyCollectionEntries {
            actual,
            maximum: MAX_PROPERTY_COLLECTION_ENTRIES,
        });
    }
    Ok(())
}

fn check_nesting_depth(actual: usize) -> Result<(), CanonicalPropertyValueError> {
    if actual > MAX_PROPERTY_NESTING_DEPTH {
        return Err(CanonicalPropertyValueError::NestingDepthExceeded {
            actual,
            maximum: MAX_PROPERTY_NESTING_DEPTH,
        });
    }
    Ok(())
}

struct PropertyValueWriter {
    bytes: Vec<u8>,
    scalar_scratch: Vec<u8>,
    expected: usize,
}

impl PropertyValueWriter {
    fn with_prefix(value_size: usize, prefix: &[u8]) -> Result<Self, CanonicalPropertyValueError> {
        let expected = prefix
            .len()
            .checked_add(value_size)
            .ok_or(CanonicalPropertyValueError::EncodedSizeOverflow)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(expected).map_err(|_| {
            CanonicalPropertyValueError::AllocationFailed {
                requested: expected,
            }
        })?;
        bytes.extend_from_slice(prefix);
        Ok(Self {
            bytes,
            scalar_scratch: Vec::new(),
            expected,
        })
    }

    fn value(
        &mut self,
        value: &CanonicalPropertyValue,
        depth: usize,
        escape_layers: usize,
    ) -> Result<(), CanonicalPropertyValueError> {
        check_nesting_depth(depth)?;
        match value {
            CanonicalPropertyValue::Scalar(scalar) => {
                scalar
                    .encode_into(&mut self.scalar_scratch)
                    .map_err(CanonicalPropertyValueError::ScalarEncode)?;
                for index in 0..self.scalar_scratch.len() {
                    let byte = self.scalar_scratch[index];
                    self.transformed_byte(byte, escape_layers)?;
                }
                Ok(())
            }
            CanonicalPropertyValue::List(list) => {
                self.transformed_byte(TAG_LIST, escape_layers)?;
                for value in list.values() {
                    self.transformed_byte(COLLECTION_ITEM, escape_layers)?;
                    self.value(value, depth + 1, escape_layers + 1)?;
                    self.transformed_byte(ORDERED_FIELD_END, escape_layers)?;
                }
                self.transformed_byte(COLLECTION_END, escape_layers)
            }
            CanonicalPropertyValue::Map(map) => {
                self.transformed_byte(TAG_MAP, escape_layers)?;
                for entry in map.entries() {
                    self.transformed_byte(COLLECTION_ITEM, escape_layers)?;
                    encode_canonical_text_into(entry.key(), &mut self.scalar_scratch)
                        .map_err(CanonicalPropertyValueError::ScalarEncode)?;
                    for index in 0..self.scalar_scratch.len() {
                        let byte = self.scalar_scratch[index];
                        self.transformed_byte(byte, escape_layers + 1)?;
                    }
                    self.transformed_byte(ORDERED_FIELD_END, escape_layers)?;
                    self.value(entry.value(), depth + 1, escape_layers + 1)?;
                    self.transformed_byte(ORDERED_FIELD_END, escape_layers)?;
                }
                self.transformed_byte(COLLECTION_END, escape_layers)
            }
        }
    }

    fn transformed_byte(
        &mut self,
        byte: u8,
        escape_layers: usize,
    ) -> Result<(), CanonicalPropertyValueError> {
        if escape_layers == 0 {
            return self.raw_byte(byte);
        }
        self.transformed_byte(ORDERED_FIELD_BYTE, escape_layers - 1)?;
        self.transformed_byte(byte, escape_layers - 1)?;
        Ok(())
    }

    fn raw_byte(&mut self, byte: u8) -> Result<(), CanonicalPropertyValueError> {
        if self.bytes.len() == self.expected {
            return Err(CanonicalPropertyValueError::EncodedSizeInvariantMismatch {
                expected: self.expected,
                actual: self.bytes.len().saturating_add(1),
            });
        }
        self.bytes.push(byte);
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>, CanonicalPropertyValueError> {
        if self.bytes.len() != self.expected {
            return Err(CanonicalPropertyValueError::EncodedSizeInvariantMismatch {
                expected: self.expected,
                actual: self.bytes.len(),
            });
        }
        Ok(self.bytes)
    }
}

fn decode_property_value<R: CanonicalScalarResolver>(
    bytes: &[u8],
    resolver: &R,
    depth: usize,
) -> Result<CanonicalPropertyValue, CanonicalPropertyValueError> {
    check_nesting_depth(depth)?;
    let (&tag, payload) = bytes
        .split_first()
        .ok_or(CanonicalPropertyValueError::EmptyEncoding)?;
    match tag {
        0x00..=0x07 => CanonicalScalar::decode_with_resolver(bytes, resolver)
            .map(CanonicalPropertyValue::Scalar)
            .map_err(CanonicalPropertyValueError::ScalarDecode),
        TAG_LIST => {
            let mut reader = PropertyValueReader::new(payload);
            let mut values = Vec::new();
            let mut child = Vec::new();
            while reader.collection_item()? {
                check_collection_count(values.len().saturating_add(1))?;
                reader.ordered_field(&mut child)?;
                let value = decode_property_value(&child, resolver, depth + 1)?;
                values.try_reserve(1).map_err(|_| {
                    CanonicalPropertyValueError::AllocationFailed {
                        requested: values.len().saturating_add(1),
                    }
                })?;
                values.push(value);
            }
            reader.finish()?;
            CanonicalList::from_canonical_decoded(values, bytes.len())
                .map(CanonicalPropertyValue::List)
        }
        TAG_MAP => {
            let mut reader = PropertyValueReader::new(payload);
            let mut entries = Vec::new();
            let mut previous_key = Vec::new();
            let mut has_previous_key = false;
            let mut field = Vec::new();
            while reader.collection_item()? {
                check_collection_count(entries.len().saturating_add(1))?;
                reader.ordered_field(&mut field)?;
                if has_previous_key {
                    match previous_key.as_slice().cmp(&field) {
                        Ordering::Less => {}
                        Ordering::Equal => {
                            return Err(CanonicalPropertyValueError::DuplicateMapKey);
                        }
                        Ordering::Greater => {
                            return Err(CanonicalPropertyValueError::NonCanonicalEncoding);
                        }
                    }
                }
                let key = match CanonicalScalar::decode_with_resolver(&field, resolver)
                    .map_err(CanonicalPropertyValueError::ScalarDecode)?
                {
                    CanonicalScalar::Text(text) => text,
                    other => {
                        return Err(CanonicalPropertyValueError::MapKeyNotText {
                            actual: CanonicalScalarKind::of(&other),
                        });
                    }
                };
                core::mem::swap(&mut previous_key, &mut field);
                has_previous_key = true;
                reader.ordered_field(&mut field)?;
                let value = decode_property_value(&field, resolver, depth + 1)?;
                entries.try_reserve(1).map_err(|_| {
                    CanonicalPropertyValueError::AllocationFailed {
                        requested: entries.len().saturating_add(1),
                    }
                })?;
                entries.push(CanonicalMapEntry::new(key, value));
            }
            reader.finish()?;
            CanonicalMap::from_canonical_decoded(entries, bytes.len())
                .map(CanonicalPropertyValue::Map)
        }
        other => Err(CanonicalPropertyValueError::UnknownTag(other)),
    }
}

struct PropertyValueReader<'a> {
    remaining: &'a [u8],
}

impl<'a> PropertyValueReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn collection_item(&mut self) -> Result<bool, CanonicalPropertyValueError> {
        let (&control, rest) = self
            .remaining
            .split_first()
            .ok_or(CanonicalPropertyValueError::MissingCollectionTerminator)?;
        self.remaining = rest;
        match control {
            COLLECTION_END => Ok(false),
            COLLECTION_ITEM => Ok(true),
            other => Err(CanonicalPropertyValueError::InvalidCollectionControl(other)),
        }
    }

    fn ordered_field(&mut self, decoded: &mut Vec<u8>) -> Result<(), CanonicalPropertyValueError> {
        let mut scan = self.remaining;
        let mut decoded_len = 0usize;
        loop {
            let (&control, rest) = scan
                .split_first()
                .ok_or(CanonicalPropertyValueError::TruncatedOrderedField)?;
            scan = rest;
            match control {
                ORDERED_FIELD_END => break,
                ORDERED_FIELD_BYTE => {
                    let (_, rest) = scan
                        .split_first()
                        .ok_or(CanonicalPropertyValueError::TruncatedOrderedField)?;
                    scan = rest;
                    decoded_len = decoded_len
                        .checked_add(1)
                        .ok_or(CanonicalPropertyValueError::EncodedSizeOverflow)?;
                    if decoded_len > MAX_PROPERTY_VALUE_BYTES {
                        return Err(CanonicalPropertyValueError::EncodedValueTooLarge {
                            actual: decoded_len,
                            maximum: MAX_PROPERTY_VALUE_BYTES,
                        });
                    }
                }
                other => {
                    return Err(CanonicalPropertyValueError::InvalidOrderedFieldControl(
                        other,
                    ));
                }
            }
        }

        let consumed = self.remaining.len() - scan.len();
        let encoded = &self.remaining[..consumed];
        decoded.clear();
        decoded.try_reserve_exact(decoded_len).map_err(|_| {
            CanonicalPropertyValueError::AllocationFailed {
                requested: decoded_len,
            }
        })?;
        let mut cursor = encoded;
        loop {
            let (&control, rest) = cursor
                .split_first()
                .ok_or(CanonicalPropertyValueError::TruncatedOrderedField)?;
            cursor = rest;
            if control == ORDERED_FIELD_END {
                break;
            }
            let (&byte, rest) = cursor
                .split_first()
                .ok_or(CanonicalPropertyValueError::TruncatedOrderedField)?;
            decoded.push(byte);
            cursor = rest;
        }
        self.remaining = scan;
        Ok(())
    }

    fn finish(self) -> Result<(), CanonicalPropertyValueError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(CanonicalPropertyValueError::TrailingBytes(
                self.remaining.len(),
            ))
        }
    }
}

/// Typed failures from profile construction, admission, and value bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CanonicalScalarProfileError {
    TooManyCollations {
        actual: usize,
        maximum: usize,
    },
    DuplicateCollation {
        object_id: ObjectId,
    },
    DescriptorSizeOverflow,
    AllocationFailed {
        requested: usize,
    },
    ProfileIdentityUnverified {
        claimed: ObjectId,
    },
    MissingArtifact {
        role: ScalarProfileArtifactRole,
        object_id: ObjectId,
    },
    ArtifactBindingMismatch {
        role: ScalarProfileArtifactRole,
        expected: ObjectId,
        actual: ObjectId,
    },
    CollationNotAdmitted {
        actual: ObjectId,
    },
    Text(CanonicalTextError),
    Timestamp(TimestampArtifactError),
    PropertyValue(CanonicalPropertyValueError),
}

impl core::fmt::Display for CanonicalScalarProfileError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooManyCollations { actual, maximum } => write!(
                formatter,
                "scalar profile contains {actual} collations, maximum {maximum}"
            ),
            Self::DuplicateCollation { object_id } => {
                write!(formatter, "scalar profile repeats collation {object_id:?}")
            }
            Self::DescriptorSizeOverflow => {
                formatter.write_str("scalar profile descriptor size overflow")
            }
            Self::AllocationFailed { requested } => write!(
                formatter,
                "unable to allocate {requested} units for scalar profile"
            ),
            Self::ProfileIdentityUnverified { claimed } => {
                write!(
                    formatter,
                    "scalar profile identity {claimed:?} was not verified"
                )
            }
            Self::MissingArtifact { role, object_id } => {
                write!(formatter, "missing {role:?} artifact {object_id:?}")
            }
            Self::ArtifactBindingMismatch {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "{role:?} binding mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::CollationNotAdmitted { actual } => {
                write!(
                    formatter,
                    "collation {actual:?} is not admitted by the profile"
                )
            }
            Self::Text(error) => write!(formatter, "canonical text rejected: {error}"),
            Self::Timestamp(error) => {
                write!(formatter, "canonical timestamp rejected: {error}")
            }
            Self::PropertyValue(error) => {
                write!(formatter, "canonical property value rejected: {error}")
            }
        }
    }
}

impl core::error::Error for CanonicalScalarProfileError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Text(error) => Some(error),
            Self::Timestamp(error) => Some(error),
            Self::PropertyValue(error) => Some(error),
            _ => None,
        }
    }
}

/// Typed exact-coercion failure.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CanonicalScalarCoercionError {
    Profile(CanonicalScalarProfileError),
    Unsupported {
        source: CanonicalScalarKind,
        target: CanonicalScalarKind,
    },
    InexactNumeric {
        source: CanonicalScalarKind,
        target: CanonicalScalarKind,
    },
    Decimal(DecimalError),
}

impl core::fmt::Display for CanonicalScalarCoercionError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Profile(error) => write!(formatter, "profile admission failed: {error}"),
            Self::Unsupported { source, target } => {
                write!(
                    formatter,
                    "unsupported scalar coercion {source:?} -> {target:?}"
                )
            }
            Self::InexactNumeric { source, target } => {
                write!(
                    formatter,
                    "inexact scalar coercion {source:?} -> {target:?}"
                )
            }
            Self::Decimal(error) => write!(formatter, "decimal coercion rejected: {error}"),
        }
    }
}

impl core::error::Error for CanonicalScalarCoercionError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Profile(error) => Some(error),
            Self::Decimal(error) => Some(error),
            _ => None,
        }
    }
}

/// Typed construction, encoding, and decoding failures for property values.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CanonicalPropertyValueError {
    EmptyEncoding,
    UnknownTag(u8),
    TooManyCollectionEntries { actual: usize, maximum: usize },
    NestingDepthExceeded { actual: usize, maximum: usize },
    DuplicateMapKey,
    MapKeyNotText { actual: CanonicalScalarKind },
    EncodedSizeOverflow,
    EncodedSizeInvariantMismatch { expected: usize, actual: usize },
    EncodedValueTooLarge { actual: usize, maximum: usize },
    AllocationFailed { requested: usize },
    MissingCollectionTerminator,
    InvalidCollectionControl(u8),
    TruncatedOrderedField,
    InvalidOrderedFieldControl(u8),
    TrailingBytes(usize),
    NonCanonicalEncoding,
    ScalarEncode(ScalarEncodeError),
    ScalarDecode(ScalarDecodeError),
}

impl core::fmt::Display for CanonicalPropertyValueError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyEncoding => formatter.write_str("empty property-value encoding"),
            Self::UnknownTag(tag) => write!(formatter, "unknown property-value tag {tag:#04x}"),
            Self::TooManyCollectionEntries { actual, maximum } => write!(
                formatter,
                "property collection contains {actual} entries, maximum {maximum}"
            ),
            Self::NestingDepthExceeded { actual, maximum } => write!(
                formatter,
                "property nesting depth {actual} exceeds maximum {maximum}"
            ),
            Self::DuplicateMapKey => {
                formatter.write_str("canonical Map contains a duplicate String key")
            }
            Self::MapKeyNotText { actual } => {
                write!(formatter, "canonical Map key is {actual:?}, expected Text")
            }
            Self::EncodedSizeOverflow => {
                formatter.write_str("property-value encoded size overflow")
            }
            Self::EncodedSizeInvariantMismatch { expected, actual } => write!(
                formatter,
                "property-value encoder produced {actual} bytes, expected {expected}"
            ),
            Self::EncodedValueTooLarge { actual, maximum } => write!(
                formatter,
                "property-value encoding is {actual} bytes, maximum {maximum}"
            ),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "unable to allocate {requested} units for property value"
            ),
            Self::MissingCollectionTerminator => {
                formatter.write_str("property collection is missing its terminator")
            }
            Self::InvalidCollectionControl(control) => write!(
                formatter,
                "invalid property collection control {control:#04x}"
            ),
            Self::TruncatedOrderedField => formatter.write_str("truncated ordered property field"),
            Self::InvalidOrderedFieldControl(control) => write!(
                formatter,
                "invalid ordered property-field control {control:#04x}"
            ),
            Self::TrailingBytes(count) => {
                write!(
                    formatter,
                    "property-value encoding has {count} trailing bytes"
                )
            }
            Self::NonCanonicalEncoding => {
                formatter.write_str("property-value bytes are not canonical")
            }
            Self::ScalarEncode(error) => write!(formatter, "scalar encoding failed: {error}"),
            Self::ScalarDecode(error) => write!(formatter, "scalar decoding failed: {error}"),
        }
    }
}

impl core::error::Error for CanonicalPropertyValueError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::ScalarEncode(error) => Some(error),
            Self::ScalarDecode(error) => Some(error),
            _ => None,
        }
    }
}
