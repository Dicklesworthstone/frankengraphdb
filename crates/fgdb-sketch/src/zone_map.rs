//! Canonical bounded byte zone maps.
//!
//! A zone map retains only the lexicographic extrema and observation count of
//! a byte-valued zone. It is an advisory pruning summary, not an exact set:
//! [`ByteZoneMap::may_contain`] can rule values out but cannot prove that an
//! interior value exists.

use core::fmt;
use std::collections::TryReserveError;

const CANONICAL_MAGIC: [u8; 8] = *b"FGDBZMP1";
const CANONICAL_VERSION: u16 = 1;
const CANONICAL_HEADER_BYTES: usize = 8 + 2 + (5 * 8);

/// Complete resource and merge profile for a byte zone map.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ZoneMapProfile {
    /// Maximum bytes accepted in one observed value.
    pub max_value_bytes: usize,
    /// Maximum number of observations represented by one map.
    pub max_observations: u64,
}

/// Caller-owned admission bounds for a canonical zone-map value.
///
/// The integration layer derives these from its scalar and object budgets;
/// encoded profile fields never authorize allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZoneMapDecodeLimits {
    /// Largest accepted encoded profile ceiling or concrete endpoint.
    pub max_endpoint_bytes: usize,
    /// Largest combined minimum-plus-maximum payload.
    pub max_total_endpoint_bytes: usize,
    /// Largest complete canonical value.
    pub max_encoded_bytes: usize,
}

/// Endpoint whose exact post-delete replacement is unknowable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZoneMapEndpoint {
    Minimum,
    Maximum,
}

/// Endpoint-related resource field checked by decode admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZoneMapDecodeResource {
    /// Profile ceiling that governs both endpoints.
    ProfileEndpointCeiling,
    /// Concrete minimum payload.
    Minimum,
    /// Concrete maximum payload.
    Maximum,
}

/// Endpoint allocation named by a fallible copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZoneMapAllocation {
    Minimum,
    Maximum,
}

/// Typed construction or state-transition failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZoneMapError {
    /// An observed value exceeds the profile's byte ceiling.
    ValueLimitExceeded { actual: usize, limit: usize },
    /// An observation or merge exceeds the profile's count ceiling.
    ObservationLimitExceeded { maximum: u64 },
    /// Adding observation counts overflowed `u64`.
    CountOverflow,
    /// The allocator rejected a checked endpoint reservation.
    AllocationFailed {
        endpoint: ZoneMapAllocation,
        requested_bytes: usize,
    },
    /// Merge operands use different complete profiles.
    ProfileMismatch {
        left: ZoneMapProfile,
        right: ZoneMapProfile,
    },
    /// No represented observation can equal the requested value.
    MissingObservation,
    /// Deleting an extremum would require information the summary does not
    /// retain. The map remains unchanged and must be rebuilt from source data.
    RebuildRequired {
        endpoint: ZoneMapEndpoint,
        count: u64,
    },
    /// The requested value lies inside the retained interval, where a zone map
    /// cannot prove that the observation exists.
    InteriorMembershipUnproven {
        /// Number of observations represented by the map.
        count: u64,
    },
    /// Private count/extrema state was internally inconsistent.
    InvariantViolation,
}

impl fmt::Display for ZoneMapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ValueLimitExceeded { actual, limit } => write!(
                formatter,
                "zone-map value has {actual} bytes, configured limit is {limit}"
            ),
            Self::ObservationLimitExceeded { maximum } => write!(
                formatter,
                "zone map reached its {maximum}-observation ceiling"
            ),
            Self::CountOverflow => formatter.write_str("zone-map observation count overflow"),
            Self::AllocationFailed {
                endpoint,
                requested_bytes,
            } => write!(
                formatter,
                "could not reserve {requested_bytes} bytes for zone-map {endpoint:?}"
            ),
            Self::ProfileMismatch { left, right } => write!(
                formatter,
                "cannot merge zone maps with profiles {left:?} and {right:?}"
            ),
            Self::MissingObservation => {
                formatter.write_str("zone map cannot contain the observation being deleted")
            }
            Self::RebuildRequired { endpoint, count } => write!(
                formatter,
                "deleting zone-map {endpoint:?} from {count} observations requires a rebuild"
            ),
            Self::InteriorMembershipUnproven { count } => write!(
                formatter,
                "deleting an interior value from {count} zone-map observations requires source reconstruction"
            ),
            Self::InvariantViolation => {
                formatter.write_str("zone-map count/extrema invariant is inconsistent")
            }
        }
    }
}

impl std::error::Error for ZoneMapError {}

/// Strict canonical-codec failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZoneMapCodecError {
    /// The decoded or in-memory state violates a zone-map law.
    State(ZoneMapError),
    /// A platform-sized field cannot be represented canonically.
    IntegerUnrepresentable,
    /// Exact encoded-size arithmetic overflowed.
    LengthOverflow,
    /// The allocator rejected the exact canonical output reservation.
    AllocationFailed {
        /// Requested canonical byte count.
        requested_bytes: usize,
    },
    /// The eight-byte format discriminator did not match.
    MagicMismatch {
        /// Bytes found at the format discriminator.
        actual: [u8; 8],
    },
    /// The encoded version is unsupported.
    UnsupportedVersion {
        /// Version found in the input.
        actual: u16,
    },
    /// Input ended before a complete field could be read.
    Truncated {
        /// Byte offset of the field.
        offset: usize,
        /// Bytes needed for the field.
        needed: usize,
        /// Bytes remaining at the offset.
        remaining: usize,
    },
    /// Input contains bytes after the one canonical value.
    TrailingBytes {
        /// First trailing byte.
        offset: usize,
        /// Number of trailing bytes.
        remaining: usize,
    },
    /// The encoded count exceeds its profile ceiling.
    ObservationLimitExceeded {
        /// Encoded count.
        actual: u64,
        /// Encoded profile ceiling.
        maximum: u64,
    },
    /// An endpoint exceeds the profile byte ceiling.
    EndpointLimitExceeded {
        /// Endpoint being validated.
        endpoint: ZoneMapEndpoint,
        /// Encoded byte length.
        actual: usize,
        /// Encoded profile ceiling.
        maximum: usize,
    },
    /// An empty map encoded endpoint bytes.
    EmptyMapHasEndpoints {
        /// Encoded minimum byte length.
        minimum_bytes: usize,
        /// Encoded maximum byte length.
        maximum_bytes: usize,
    },
    /// The minimum sorts after the maximum.
    ExtremaOutOfOrder,
    /// A singleton map encoded two different extrema.
    SingletonExtremaDiffer,
    /// The complete encoded value exceeds the caller-owned byte budget.
    EncodedByteLimitExceeded {
        /// Input byte length.
        actual: usize,
        /// Caller-owned byte ceiling.
        maximum: usize,
    },
    /// An encoded profile ceiling or endpoint exceeds caller-owned admission.
    DecodeEndpointLimitExceeded {
        /// Encoded resource that exceeded admission.
        resource: ZoneMapDecodeResource,
        /// Encoded byte ceiling or concrete endpoint length.
        actual: usize,
        /// Caller-owned endpoint ceiling.
        maximum: usize,
    },
    /// Combined encoded endpoint bytes exceed caller-owned admission.
    DecodeTotalEndpointLimitExceeded {
        /// Combined endpoint bytes.
        actual: usize,
        /// Caller-owned combined ceiling.
        maximum: usize,
    },
    /// The encoded profile is not the registry-selected profile.
    ProfileMismatch {
        /// Profile selected by trusted metadata.
        expected: ZoneMapProfile,
        /// Profile found in the canonical value.
        actual: ZoneMapProfile,
    },
}

impl fmt::Display for ZoneMapCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ZoneMapCodecError {}

impl From<ZoneMapError> for ZoneMapCodecError {
    fn from(error: ZoneMapError) -> Self {
        Self::State(error)
    }
}

/// Borrowed canonical logical state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZoneMapState<'map> {
    pub profile: ZoneMapProfile,
    pub count: u64,
    pub minimum: Option<&'map [u8]>,
    pub maximum: Option<&'map [u8]>,
}

/// Mergeable lexicographic extrema over bounded owned byte values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ByteZoneMap {
    profile: ZoneMapProfile,
    count: u64,
    minimum: Option<Vec<u8>>,
    maximum: Option<Vec<u8>>,
}

impl ByteZoneMap {
    /// Creates an empty map without allocating.
    #[must_use]
    pub const fn new(profile: ZoneMapProfile) -> Self {
        Self {
            profile,
            count: 0,
            minimum: None,
            maximum: None,
        }
    }

    /// Returns the complete merge and resource profile.
    #[must_use]
    pub const fn profile(&self) -> ZoneMapProfile {
        self.profile
    }

    /// Returns the represented observation count.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.count
    }

    /// Returns whether no observations are represented.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns the lexicographically least observed bytes.
    #[must_use]
    pub fn minimum(&self) -> Option<&[u8]> {
        self.minimum.as_deref()
    }

    /// Returns the lexicographically greatest observed bytes.
    #[must_use]
    pub fn maximum(&self) -> Option<&[u8]> {
        self.maximum.as_deref()
    }

    /// Returns the canonical logical state.
    #[must_use]
    pub fn canonical_state(&self) -> ZoneMapState<'_> {
        ZoneMapState {
            profile: self.profile,
            count: self.count,
            minimum: self.minimum(),
            maximum: self.maximum(),
        }
    }

    /// Encodes the complete profile, count, and extrema into canonical bytes.
    /// Fixed-width integers use the workspace's little-endian durable law.
    pub fn try_to_canonical_bytes(&self) -> Result<Vec<u8>, ZoneMapCodecError> {
        validate_canonical_zone_map(
            self.profile,
            self.count,
            self.minimum.as_deref(),
            self.maximum.as_deref(),
        )?;
        let minimum_bytes = self.minimum.as_deref().map_or(0, <[u8]>::len);
        let maximum_bytes = self.maximum.as_deref().map_or(0, <[u8]>::len);
        let endpoint_bytes = minimum_bytes
            .checked_add(maximum_bytes)
            .ok_or(ZoneMapCodecError::LengthOverflow)?;
        let encoded_len = CANONICAL_HEADER_BYTES
            .checked_add(endpoint_bytes)
            .ok_or(ZoneMapCodecError::LengthOverflow)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(encoded_len)
            .map_err(|_: TryReserveError| ZoneMapCodecError::AllocationFailed {
                requested_bytes: encoded_len,
            })?;
        bytes.extend_from_slice(&CANONICAL_MAGIC);
        push_u16(&mut bytes, CANONICAL_VERSION);
        push_u64(
            &mut bytes,
            u64::try_from(self.profile.max_value_bytes)
                .map_err(|_| ZoneMapCodecError::IntegerUnrepresentable)?,
        );
        push_u64(&mut bytes, self.profile.max_observations);
        push_u64(&mut bytes, self.count);
        push_u64(
            &mut bytes,
            u64::try_from(minimum_bytes).map_err(|_| ZoneMapCodecError::IntegerUnrepresentable)?,
        );
        push_u64(
            &mut bytes,
            u64::try_from(maximum_bytes).map_err(|_| ZoneMapCodecError::IntegerUnrepresentable)?,
        );
        if let Some(minimum) = &self.minimum {
            bytes.extend_from_slice(minimum);
        }
        if let Some(maximum) = &self.maximum {
            bytes.extend_from_slice(maximum);
        }
        debug_assert_eq!(bytes.len(), encoded_len);
        Ok(bytes)
    }

    /// Decodes exactly one canonical zone-map value.
    ///
    /// Profile, endpoint lengths, and exact input length are checked before
    /// either endpoint is allocated.
    pub fn try_from_canonical_bytes(
        bytes: &[u8],
        expected_profile: ZoneMapProfile,
        limits: ZoneMapDecodeLimits,
    ) -> Result<Self, ZoneMapCodecError> {
        if bytes.len() > limits.max_encoded_bytes {
            return Err(ZoneMapCodecError::EncodedByteLimitExceeded {
                actual: bytes.len(),
                maximum: limits.max_encoded_bytes,
            });
        }
        let mut decoder = ZoneMapDecoder::new(bytes);
        let magic = decoder.read_array::<8>()?;
        if magic != CANONICAL_MAGIC {
            return Err(ZoneMapCodecError::MagicMismatch { actual: magic });
        }
        let version = decoder.read_u16()?;
        if version != CANONICAL_VERSION {
            return Err(ZoneMapCodecError::UnsupportedVersion { actual: version });
        }
        let max_value_bytes = usize::try_from(decoder.read_u64()?)
            .map_err(|_| ZoneMapCodecError::IntegerUnrepresentable)?;
        let max_observations = decoder.read_u64()?;
        let count = decoder.read_u64()?;
        let minimum_bytes = usize::try_from(decoder.read_u64()?)
            .map_err(|_| ZoneMapCodecError::IntegerUnrepresentable)?;
        let maximum_bytes = usize::try_from(decoder.read_u64()?)
            .map_err(|_| ZoneMapCodecError::IntegerUnrepresentable)?;
        let profile = ZoneMapProfile {
            max_value_bytes,
            max_observations,
        };
        if profile != expected_profile {
            return Err(ZoneMapCodecError::ProfileMismatch {
                expected: expected_profile,
                actual: profile,
            });
        }
        if max_value_bytes > limits.max_endpoint_bytes {
            return Err(ZoneMapCodecError::DecodeEndpointLimitExceeded {
                resource: ZoneMapDecodeResource::ProfileEndpointCeiling,
                actual: max_value_bytes,
                maximum: limits.max_endpoint_bytes,
            });
        }
        if minimum_bytes > limits.max_endpoint_bytes {
            return Err(ZoneMapCodecError::DecodeEndpointLimitExceeded {
                resource: ZoneMapDecodeResource::Minimum,
                actual: minimum_bytes,
                maximum: limits.max_endpoint_bytes,
            });
        }
        if maximum_bytes > limits.max_endpoint_bytes {
            return Err(ZoneMapCodecError::DecodeEndpointLimitExceeded {
                resource: ZoneMapDecodeResource::Maximum,
                actual: maximum_bytes,
                maximum: limits.max_endpoint_bytes,
            });
        }
        let endpoint_bytes = minimum_bytes
            .checked_add(maximum_bytes)
            .ok_or(ZoneMapCodecError::LengthOverflow)?;
        if endpoint_bytes > limits.max_total_endpoint_bytes {
            return Err(ZoneMapCodecError::DecodeTotalEndpointLimitExceeded {
                actual: endpoint_bytes,
                maximum: limits.max_total_endpoint_bytes,
            });
        }
        validate_encoded_zone_map_bounds(
            max_value_bytes,
            max_observations,
            count,
            minimum_bytes,
            maximum_bytes,
        )?;
        let expected_len = CANONICAL_HEADER_BYTES
            .checked_add(endpoint_bytes)
            .ok_or(ZoneMapCodecError::LengthOverflow)?;
        if bytes.len() < expected_len {
            return Err(ZoneMapCodecError::Truncated {
                offset: decoder.offset,
                needed: endpoint_bytes,
                remaining: bytes.len().saturating_sub(decoder.offset),
            });
        }
        if bytes.len() > expected_len {
            return Err(ZoneMapCodecError::TrailingBytes {
                offset: expected_len,
                remaining: bytes.len() - expected_len,
            });
        }

        if count == 0 {
            decoder.finish()?;
            return Ok(Self::new(profile));
        }

        let payload_offset = decoder.offset;
        let minimum_source = decoder.read_bytes(minimum_bytes)?;
        let maximum_source = decoder.read_bytes(maximum_bytes)?;
        validate_canonical_zone_map(profile, count, Some(minimum_source), Some(maximum_source))?;
        decoder.finish()?;

        let mut materialize = ZoneMapDecoder {
            bytes,
            offset: payload_offset,
        };
        let minimum = try_copy_endpoint(
            materialize.read_bytes(minimum_bytes)?,
            ZoneMapAllocation::Minimum,
        )?;
        let maximum = try_copy_endpoint(
            materialize.read_bytes(maximum_bytes)?,
            ZoneMapAllocation::Maximum,
        )?;
        materialize.finish()?;
        Ok(Self {
            profile,
            count,
            minimum: Some(minimum),
            maximum: Some(maximum),
        })
    }

    /// Adds one observation after preflighting every allocation and count
    /// transition. Failure leaves the map byte-for-byte unchanged.
    pub fn try_observe(&mut self, value: &[u8]) -> Result<(), ZoneMapError> {
        self.validate_value(value)?;
        let next_count = self.checked_combined_count(1)?;

        if self.count == 0 {
            let minimum = try_copy_endpoint(value, ZoneMapAllocation::Minimum)?;
            let maximum = try_copy_endpoint(value, ZoneMapAllocation::Maximum)?;
            self.minimum = Some(minimum);
            self.maximum = Some(maximum);
            self.count = next_count;
            return Ok(());
        }

        let (Some(minimum), Some(maximum)) = (&self.minimum, &self.maximum) else {
            return Err(ZoneMapError::InvariantViolation);
        };
        let next_minimum = (value < minimum.as_slice())
            .then(|| try_copy_endpoint(value, ZoneMapAllocation::Minimum))
            .transpose()?;
        let next_maximum = (value > maximum.as_slice())
            .then(|| try_copy_endpoint(value, ZoneMapAllocation::Maximum))
            .transpose()?;

        if let Some(minimum) = next_minimum {
            self.minimum = Some(minimum);
        }
        if let Some(maximum) = next_maximum {
            self.maximum = Some(maximum);
        }
        self.count = next_count;
        Ok(())
    }

    /// Merges another map with the identical complete profile.
    ///
    /// Both replacement extrema and the resulting count are prepared before
    /// mutation, making profile, limit, and allocation failures atomic.
    pub fn try_merge(&mut self, other: &Self) -> Result<(), ZoneMapError> {
        if self.profile != other.profile {
            return Err(ZoneMapError::ProfileMismatch {
                left: self.profile,
                right: other.profile,
            });
        }
        if other.count == 0 {
            return Ok(());
        }
        let next_count = self.checked_combined_count(other.count)?;
        let (Some(other_minimum), Some(other_maximum)) = (&other.minimum, &other.maximum) else {
            return Err(ZoneMapError::InvariantViolation);
        };

        let replace_minimum = self
            .minimum
            .as_deref()
            .is_none_or(|minimum| other_minimum.as_slice() < minimum);
        let replace_maximum = self
            .maximum
            .as_deref()
            .is_none_or(|maximum| other_maximum.as_slice() > maximum);
        let next_minimum = replace_minimum
            .then(|| try_copy_endpoint(other_minimum, ZoneMapAllocation::Minimum))
            .transpose()?;
        let next_maximum = replace_maximum
            .then(|| try_copy_endpoint(other_maximum, ZoneMapAllocation::Maximum))
            .transpose()?;

        if let Some(minimum) = next_minimum {
            self.minimum = Some(minimum);
        }
        if let Some(maximum) = next_maximum {
            self.maximum = Some(maximum);
        }
        self.count = next_count;
        Ok(())
    }

    /// Removes one observation when its membership and extrema transition are
    /// exactly derivable from retained state.
    ///
    /// Equal extrema prove that every represented value is identical. Two
    /// distinct extrema with count two prove that each endpoint occurs exactly
    /// once. Other endpoint deletes return [`ZoneMapError::RebuildRequired`];
    /// other interior deletes return
    /// [`ZoneMapError::InteriorMembershipUnproven`]. Both leave the map
    /// unchanged because it cannot prove membership or a replacement extremum.
    pub fn try_remove(&mut self, value: &[u8]) -> Result<(), ZoneMapError> {
        self.validate_value(value)?;
        let (Some(minimum), Some(maximum)) = (&self.minimum, &self.maximum) else {
            return if self.count == 0 {
                Err(ZoneMapError::MissingObservation)
            } else {
                Err(ZoneMapError::InvariantViolation)
            };
        };
        if self.count == 0 {
            return Err(ZoneMapError::InvariantViolation);
        }
        if value < minimum.as_slice() || value > maximum.as_slice() {
            return Err(ZoneMapError::MissingObservation);
        }

        if minimum == maximum {
            if value != minimum.as_slice() {
                return Err(ZoneMapError::MissingObservation);
            }
            self.count -= 1;
            if self.count == 0 {
                self.minimum = None;
                self.maximum = None;
            }
            return Ok(());
        }

        if value == minimum.as_slice() {
            if self.count != 2 {
                return Err(ZoneMapError::RebuildRequired {
                    endpoint: ZoneMapEndpoint::Minimum,
                    count: self.count,
                });
            }
            let replacement = try_copy_endpoint(maximum, ZoneMapAllocation::Minimum)?;
            self.minimum = Some(replacement);
            self.count = 1;
            return Ok(());
        }
        if value == maximum.as_slice() {
            if self.count != 2 {
                return Err(ZoneMapError::RebuildRequired {
                    endpoint: ZoneMapEndpoint::Maximum,
                    count: self.count,
                });
            }
            let replacement = try_copy_endpoint(minimum, ZoneMapAllocation::Maximum)?;
            self.maximum = Some(replacement);
            self.count = 1;
            return Ok(());
        }

        if self.count <= 2 {
            return Err(ZoneMapError::MissingObservation);
        }
        Err(ZoneMapError::InteriorMembershipUnproven { count: self.count })
    }

    /// Returns whether the retained interval permits `value`.
    ///
    /// `false` is definitive; `true` means only that the value lies between
    /// the extrema.
    #[must_use]
    pub fn may_contain(&self, value: &[u8]) -> bool {
        self.minimum
            .as_deref()
            .zip(self.maximum.as_deref())
            .is_some_and(|(minimum, maximum)| minimum <= value && value <= maximum)
    }

    /// Returns whether the retained interval fully covers an ordered,
    /// inclusive query interval.
    #[must_use]
    pub fn covers_range(&self, lower: &[u8], upper: &[u8]) -> bool {
        lower <= upper
            && self
                .minimum
                .as_deref()
                .zip(self.maximum.as_deref())
                .is_some_and(|(minimum, maximum)| minimum <= lower && upper <= maximum)
    }

    /// Returns whether the retained interval overlaps an ordered, inclusive
    /// query interval.
    #[must_use]
    pub fn may_overlap_range(&self, lower: &[u8], upper: &[u8]) -> bool {
        lower <= upper
            && self
                .minimum
                .as_deref()
                .zip(self.maximum.as_deref())
                .is_some_and(|(minimum, maximum)| minimum <= upper && lower <= maximum)
    }

    fn validate_value(&self, value: &[u8]) -> Result<(), ZoneMapError> {
        if value.len() > self.profile.max_value_bytes {
            return Err(ZoneMapError::ValueLimitExceeded {
                actual: value.len(),
                limit: self.profile.max_value_bytes,
            });
        }
        Ok(())
    }

    fn checked_combined_count(&self, additional: u64) -> Result<u64, ZoneMapError> {
        let combined = self
            .count
            .checked_add(additional)
            .ok_or(ZoneMapError::CountOverflow)?;
        if combined > self.profile.max_observations {
            return Err(ZoneMapError::ObservationLimitExceeded {
                maximum: self.profile.max_observations,
            });
        }
        Ok(combined)
    }
}

fn try_copy_endpoint(value: &[u8], endpoint: ZoneMapAllocation) -> Result<Vec<u8>, ZoneMapError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_: TryReserveError| ZoneMapError::AllocationFailed {
            endpoint,
            requested_bytes: value.len(),
        })?;
    owned.extend_from_slice(value);
    Ok(owned)
}

fn validate_encoded_zone_map_bounds(
    max_value_bytes: usize,
    max_observations: u64,
    count: u64,
    minimum_bytes: usize,
    maximum_bytes: usize,
) -> Result<(), ZoneMapCodecError> {
    if count > max_observations {
        return Err(ZoneMapCodecError::ObservationLimitExceeded {
            actual: count,
            maximum: max_observations,
        });
    }
    if minimum_bytes > max_value_bytes {
        return Err(ZoneMapCodecError::EndpointLimitExceeded {
            endpoint: ZoneMapEndpoint::Minimum,
            actual: minimum_bytes,
            maximum: max_value_bytes,
        });
    }
    if maximum_bytes > max_value_bytes {
        return Err(ZoneMapCodecError::EndpointLimitExceeded {
            endpoint: ZoneMapEndpoint::Maximum,
            actual: maximum_bytes,
            maximum: max_value_bytes,
        });
    }
    if count == 0 && (minimum_bytes != 0 || maximum_bytes != 0) {
        return Err(ZoneMapCodecError::EmptyMapHasEndpoints {
            minimum_bytes,
            maximum_bytes,
        });
    }
    Ok(())
}

fn validate_canonical_zone_map(
    profile: ZoneMapProfile,
    count: u64,
    minimum: Option<&[u8]>,
    maximum: Option<&[u8]>,
) -> Result<(), ZoneMapCodecError> {
    let minimum_bytes = minimum.map_or(0, <[u8]>::len);
    let maximum_bytes = maximum.map_or(0, <[u8]>::len);
    validate_encoded_zone_map_bounds(
        profile.max_value_bytes,
        profile.max_observations,
        count,
        minimum_bytes,
        maximum_bytes,
    )?;
    match (count, minimum, maximum) {
        (0, None, None) => Ok(()),
        (0, _, _) | (_, None, _) | (_, _, None) => {
            Err(ZoneMapCodecError::State(ZoneMapError::InvariantViolation))
        }
        (1, Some(minimum), Some(maximum)) if minimum != maximum => {
            Err(ZoneMapCodecError::SingletonExtremaDiffer)
        }
        (_, Some(minimum), Some(maximum)) if minimum > maximum => {
            Err(ZoneMapCodecError::ExtremaOutOfOrder)
        }
        _ => Ok(()),
    }
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

struct ZoneMapDecoder<'bytes> {
    bytes: &'bytes [u8],
    offset: usize,
}

impl<'bytes> ZoneMapDecoder<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_bytes(&mut self, length: usize) -> Result<&'bytes [u8], ZoneMapCodecError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(ZoneMapCodecError::LengthOverflow)?;
        let Some(value) = self.bytes.get(self.offset..end) else {
            return Err(ZoneMapCodecError::Truncated {
                offset: self.offset,
                needed: length,
                remaining: self.bytes.len().saturating_sub(self.offset),
            });
        };
        self.offset = end;
        Ok(value)
    }

    fn read_array<const LENGTH: usize>(&mut self) -> Result<[u8; LENGTH], ZoneMapCodecError> {
        let source = self.read_bytes(LENGTH)?;
        let mut value = [0_u8; LENGTH];
        value.copy_from_slice(source);
        Ok(value)
    }

    fn read_u16(&mut self) -> Result<u16, ZoneMapCodecError> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_u64(&mut self) -> Result<u64, ZoneMapCodecError> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    fn finish(self) -> Result<(), ZoneMapCodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(ZoneMapCodecError::TrailingBytes {
                offset: self.offset,
                remaining: self.bytes.len() - self.offset,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> ZoneMapProfile {
        ZoneMapProfile {
            max_value_bytes: 16,
            max_observations: 100,
        }
    }

    fn map(values: &[&[u8]]) -> ByteZoneMap {
        let mut map = ByteZoneMap::new(profile());
        for &value in values {
            map.try_observe(value).expect("bounded fixture");
        }
        map
    }

    fn decode_limits() -> ZoneMapDecodeLimits {
        ZoneMapDecodeLimits {
            max_endpoint_bytes: profile().max_value_bytes,
            max_total_endpoint_bytes: profile().max_value_bytes * 2,
            max_encoded_bytes: CANONICAL_HEADER_BYTES + (profile().max_value_bytes * 2),
        }
    }

    fn read_fixture(bytes: &[u8]) -> Result<ByteZoneMap, ZoneMapCodecError> {
        ByteZoneMap::try_from_canonical_bytes(bytes, profile(), decode_limits())
    }

    #[test]
    fn byte_order_extrema_and_containment_are_canonical() {
        let mut zone = ByteZoneMap::new(profile());
        for value in [
            &b"middle"[..],
            &b""[..],
            &b"mid"[..],
            &b"\xff"[..],
            &b"z"[..],
        ] {
            zone.try_observe(value).expect("bounded fixture");
        }
        assert_eq!(zone.len(), 5);
        assert_eq!(zone.minimum(), Some(&b""[..]));
        assert_eq!(zone.maximum(), Some(&b"\xff"[..]));
        assert!(zone.may_contain(b"alpha"));
        assert!(zone.may_contain(b"\xff"));
        assert!(zone.covers_range(b"mid", b"z"));
        assert!(zone.may_overlap_range(b"z", b"\xff\xff"));
        assert!(!zone.covers_range(b"z", b"a"));
        assert!(!zone.may_overlap_range(b"\xff\x01", b"\xff\xff"));

        let empty = ByteZoneMap::new(profile());
        assert!(!empty.may_contain(b""));
        assert!(!empty.covers_range(b"", b""));
    }

    #[test]
    fn canonical_codec_round_trips_empty_singleton_and_merged_states() {
        let empty = ByteZoneMap::new(profile());
        let empty_bytes = empty
            .try_to_canonical_bytes()
            .expect("valid empty zone map");
        assert_eq!(
            read_fixture(&empty_bytes).expect("canonical empty map"),
            empty
        );

        let singleton = map(&[b"same", b"same"]);
        let singleton_bytes = singleton
            .try_to_canonical_bytes()
            .expect("valid repeated singleton range");
        assert_eq!(
            read_fixture(&singleton_bytes).expect("canonical singleton range"),
            singleton
        );

        let forward = map(&[b"m", b"z", b"a", b"x"]);
        let reverse = map(&[b"x", b"a", b"z", b"m"]);
        let forward_bytes = forward.try_to_canonical_bytes().expect("valid zone map");
        let reverse_bytes = reverse.try_to_canonical_bytes().expect("valid zone map");
        assert_eq!(forward_bytes, reverse_bytes);
        assert_eq!(&forward_bytes[..8], b"FGDBZMP1");
        assert_eq!(&forward_bytes[8..10], &1_u16.to_le_bytes());
        let decoded = read_fixture(&forward_bytes).expect("canonical zone map");
        assert_eq!(decoded, forward);
        assert_eq!(
            decoded.try_to_canonical_bytes().expect("decoded zone map"),
            forward_bytes
        );
    }

    #[test]
    fn canonical_decoder_rejects_incomplete_or_invalid_extrema() {
        let encoded = map(&[b"a", b"z"])
            .try_to_canonical_bytes()
            .expect("valid zone map");

        let mut wrong_magic = encoded.clone();
        wrong_magic[0] ^= 1;
        assert!(matches!(
            read_fixture(&wrong_magic),
            Err(ZoneMapCodecError::MagicMismatch { .. })
        ));

        let mut wrong_version = encoded.clone();
        wrong_version[8..10].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            read_fixture(&wrong_version),
            Err(ZoneMapCodecError::UnsupportedVersion { actual: 2 })
        );

        let mut reversed = encoded.clone();
        reversed[50] = b'z';
        reversed[51] = b'a';
        assert_eq!(
            read_fixture(&reversed),
            Err(ZoneMapCodecError::ExtremaOutOfOrder)
        );

        let mut singleton = map(&[b"a"])
            .try_to_canonical_bytes()
            .expect("valid singleton");
        singleton[51] = b'b';
        assert_eq!(
            read_fixture(&singleton),
            Err(ZoneMapCodecError::SingletonExtremaDiffer)
        );

        let mut empty_with_endpoint = ByteZoneMap::new(profile())
            .try_to_canonical_bytes()
            .expect("valid empty map");
        empty_with_endpoint[34..42].copy_from_slice(&1_u64.to_le_bytes());
        empty_with_endpoint.push(b'a');
        assert_eq!(
            read_fixture(&empty_with_endpoint),
            Err(ZoneMapCodecError::EmptyMapHasEndpoints {
                minimum_bytes: 1,
                maximum_bytes: 0,
            })
        );

        assert!(matches!(
            read_fixture(&encoded[..encoded.len() - 1]),
            Err(ZoneMapCodecError::Truncated { .. })
        ));

        let mut trailing = encoded;
        trailing.push(0);
        assert!(matches!(
            read_fixture(&trailing),
            Err(ZoneMapCodecError::TrailingBytes { remaining: 1, .. })
        ));
    }

    #[test]
    fn canonical_decoder_enforces_trusted_profile_and_resource_bounds() {
        let encoded = map(&[b"a", b"z"])
            .try_to_canonical_bytes()
            .expect("valid zone map");

        let different_profile = ZoneMapProfile {
            max_value_bytes: profile().max_value_bytes - 1,
            ..profile()
        };
        assert!(matches!(
            ByteZoneMap::try_from_canonical_bytes(&encoded, different_profile, decode_limits(),),
            Err(ZoneMapCodecError::ProfileMismatch { .. })
        ));

        let limits = ZoneMapDecodeLimits {
            max_endpoint_bytes: profile().max_value_bytes - 1,
            ..decode_limits()
        };
        assert_eq!(
            ByteZoneMap::try_from_canonical_bytes(&encoded, profile(), limits),
            Err(ZoneMapCodecError::DecodeEndpointLimitExceeded {
                resource: ZoneMapDecodeResource::ProfileEndpointCeiling,
                actual: profile().max_value_bytes,
                maximum: profile().max_value_bytes - 1,
            })
        );

        let limits = ZoneMapDecodeLimits {
            max_encoded_bytes: encoded.len() - 1,
            ..decode_limits()
        };
        assert_eq!(
            ByteZoneMap::try_from_canonical_bytes(&encoded, profile(), limits),
            Err(ZoneMapCodecError::EncodedByteLimitExceeded {
                actual: encoded.len(),
                maximum: encoded.len() - 1,
            })
        );
    }

    #[test]
    fn observation_limits_fail_without_mutation() {
        let mut zone = ByteZoneMap::new(ZoneMapProfile {
            max_value_bytes: 2,
            max_observations: 2,
        });
        zone.try_observe(b"b").expect("first value");
        let before = zone.clone();
        assert_eq!(
            zone.try_observe(b"long"),
            Err(ZoneMapError::ValueLimitExceeded {
                actual: 4,
                limit: 2,
            })
        );
        assert_eq!(zone, before);
        zone.try_observe(b"a").expect("second value");
        let before = zone.clone();
        assert_eq!(
            zone.try_observe(b"c"),
            Err(ZoneMapError::ObservationLimitExceeded { maximum: 2 })
        );
        assert_eq!(zone, before);
    }

    #[test]
    fn identical_profile_merge_is_order_independent() {
        let left = map(&[b"m", b"z"]);
        let right = map(&[b"a", b"x", b"y"]);
        let mut left_right = left.clone();
        left_right.try_merge(&right).expect("identical profile");
        let mut right_left = right;
        right_left.try_merge(&left).expect("identical profile");
        assert_eq!(left_right, right_left);
        assert_eq!(left_right.len(), 5);
        assert_eq!(left_right.minimum(), Some(&b"a"[..]));
        assert_eq!(left_right.maximum(), Some(&b"z"[..]));

        let mut identity = ByteZoneMap::new(profile());
        identity.try_merge(&left_right).expect("identical profile");
        assert_eq!(identity, left_right);
    }

    #[test]
    fn merge_profile_and_count_failures_are_atomic() {
        let mut zone = ByteZoneMap::new(ZoneMapProfile {
            max_value_bytes: 8,
            max_observations: 2,
        });
        zone.try_observe(b"m").expect("within limit");
        let different = ByteZoneMap::new(ZoneMapProfile {
            max_value_bytes: 9,
            max_observations: 2,
        });
        let before = zone.clone();
        assert_eq!(
            zone.try_merge(&different),
            Err(ZoneMapError::ProfileMismatch {
                left: ZoneMapProfile {
                    max_value_bytes: 8,
                    max_observations: 2,
                },
                right: ZoneMapProfile {
                    max_value_bytes: 9,
                    max_observations: 2,
                },
            })
        );
        assert_eq!(zone, before);

        let other = {
            let mut other = ByteZoneMap::new(zone.profile());
            other.try_observe(b"a").expect("within limit");
            other.try_observe(b"z").expect("within limit");
            other
        };
        assert_eq!(
            zone.try_merge(&other),
            Err(ZoneMapError::ObservationLimitExceeded { maximum: 2 })
        );
        assert_eq!(zone, before);
    }

    #[test]
    fn deletes_are_exact_when_membership_and_extrema_are_derivable() {
        let mut repeated = map(&[b"k", b"k", b"k"]);
        repeated.try_remove(b"k").expect("same extrema remain");
        assert_eq!(repeated.len(), 2);
        assert_eq!(repeated.minimum(), Some(&b"k"[..]));
        repeated.try_remove(b"k").expect("same extrema remain");
        repeated.try_remove(b"k").expect("last value empties map");
        assert!(repeated.is_empty());
        assert_eq!(repeated.minimum(), None);
        assert_eq!(repeated.maximum(), None);

        let mut distinct = map(&[b"a", b"z"]);
        distinct.try_remove(b"a").expect("two endpoints are exact");
        assert_eq!(distinct.len(), 1);
        assert_eq!(distinct.minimum(), Some(&b"z"[..]));
        assert_eq!(distinct.maximum(), Some(&b"z"[..]));
    }

    #[test]
    fn interior_delete_requires_source_reconstruction_atomically() {
        let mut zone = map(&[b"a", b"x", b"z"]);
        let before = zone.clone();
        assert_eq!(
            zone.try_remove(b"m"),
            Err(ZoneMapError::InteriorMembershipUnproven { count: 3 })
        );
        assert_eq!(zone, before);
        assert_eq!(
            zone.try_remove(b"x"),
            Err(ZoneMapError::InteriorMembershipUnproven { count: 3 })
        );
        assert_eq!(zone, before);
    }

    #[test]
    fn ambiguous_endpoint_delete_requires_rebuild_atomically() {
        let mut zone = map(&[b"a", b"m", b"z", b"z"]);
        let before = zone.clone();
        assert_eq!(
            zone.try_remove(b"a"),
            Err(ZoneMapError::RebuildRequired {
                endpoint: ZoneMapEndpoint::Minimum,
                count: 4,
            })
        );
        assert_eq!(zone, before);
        assert_eq!(
            zone.try_remove(b"z"),
            Err(ZoneMapError::RebuildRequired {
                endpoint: ZoneMapEndpoint::Maximum,
                count: 4,
            })
        );
        assert_eq!(zone, before);
    }

    #[test]
    fn impossible_deletes_are_rejected_without_drift() {
        let mut empty = ByteZoneMap::new(profile());
        assert_eq!(
            empty.try_remove(b"a"),
            Err(ZoneMapError::MissingObservation)
        );

        let mut zone = map(&[b"a", b"z"]);
        let before = zone.clone();
        assert_eq!(zone.try_remove(b"m"), Err(ZoneMapError::MissingObservation));
        assert_eq!(
            zone.try_remove(b"\xff"),
            Err(ZoneMapError::MissingObservation)
        );
        assert_eq!(zone, before);
    }
}
