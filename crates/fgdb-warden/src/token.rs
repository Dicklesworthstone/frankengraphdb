//! Foundation-backed authentication and bounded, canonical caveat compilation.

use crate::{
    Error, ExecutionPermit, Grant, MAX_CAVEATS, MAX_SCOPE_ORDINALS, MAX_TOKEN_BYTES,
    MAX_VALUE_BYTES, PlannerPredicates, QueryLimits, ReadAccess, Restriction, Rights, Scope,
    WriteAccess, validate_name,
};
use asupersync::cx::macaroon::{Caveat, CaveatPredicate, MacaroonToken};
use asupersync::security::key::AuthKey;
use core::fmt;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

const LOCATION: &str = "fgdb/warden/v1";
const MAX_IDENTIFIER_BYTES: usize = 1024;
const BRANCH: &str = "fgdb/warden/v1/branch";
const LABELS: &str = "fgdb/warden/v1/labels";
const RELATIONS: &str = "fgdb/warden/v1/relations";
const PROPERTIES: &str = "fgdb/warden/v1/properties";
const DENY_PROPERTIES: &str = "fgdb/warden/v1/deny-properties";
const RIGHTS: &str = "fgdb/warden/v1/rights";
const MAX_NODES: &str = "fgdb/warden/v1/max-nodes";
const MAX_WORK: &str = "fgdb/warden/v1/max-work";
const MAX_ROWS: &str = "fgdb/warden/v1/max-rows";

/// Opaque UNVERIFIED bearer material. Its Debug representation is redacted.
///
/// The signature is itself an attenuation key. Never use it as a public plan
/// cache key, certificate field, log identifier, or telemetry label.
#[derive(Clone)]
pub struct CapabilityToken {
    token: MacaroonToken,
}

impl fmt::Debug for CapabilityToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CapabilityToken(<redacted>)")
    }
}

impl CapabilityToken {
    /// Decode a bounded, canonical token. Authentication is a SEPARATE step.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_TOKEN_BYTES {
            return Err(Error::TooLarge);
        }
        let token = MacaroonToken::from_binary(bytes).ok_or(Error::Malformed)?;
        validate_shape(&token)?;
        // The pinned foundation reader accepts a decoded predicate even when
        // its enclosing predicate packet contains unused bytes. Reject that
        // alternate wire spelling; those bytes are not in the HMAC chain.
        // This also detects a declared caveat count that the reader clamped.
        if token.to_binary() != bytes {
            return Err(Error::Malformed);
        }
        Ok(Self { token })
    }

    /// Returns secret bearer bytes for explicit transport only.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        self.token.to_binary()
    }

    /// Append a restriction without the root key, preserving the parent.
    /// This does NOT authenticate the input token or produce a verified type.
    pub fn attenuate(&self, restriction: Restriction) -> Result<Self, Error> {
        if self.token.caveat_count() >= MAX_CAVEATS {
            return Err(Error::TooLarge);
        }
        let predicate = encode_restriction(&restriction)?;
        let token = self.token.clone().add_caveat(predicate);
        validate_shape(&token)?;
        if token.to_binary().len() > MAX_TOKEN_BYTES {
            return Err(Error::TooLarge);
        }
        Ok(Self { token })
    }
}

/// A host-owned key and exact logical authority identity.
///
/// The signed identifier binds the database security namespace, graph name,
/// catalog epoch and policy epoch. Location hints are deliberately NOT used
/// for authorization: the foundation does not authenticate those hints.
/// Changing either epoch rejects earlier tokens on subsequent admissions.
/// Retire the old Authority when installing a new catalog/policy epoch.
/// Retirement stops subsequent admission and is observed by already borrowed
/// permits at their next checkpoint. This is a local, cooperative fence, not
/// a durable revocation registry or a barrier against uncheckpointed effects.
pub struct Authority {
    key: AuthKey,
    identifier: String,
    namespace: DatabaseSecurityNamespaceId,
    retired: AtomicBool,
}

impl fmt::Debug for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Authority(<redacted>)")
    }
}

impl Authority {
    /// Key material must come from the host's secret-management/CSPRNG path.
    /// No seed expansion or test-key fallback is performed by this API.
    pub fn new(
        key: AuthKey,
        namespace: DatabaseSecurityNamespaceId,
        graph: &str,
        schema_epoch: SchemaEpoch,
        policy_epoch: u64,
    ) -> Result<Self, Error> {
        validate_name(graph)?;
        let identifier = format!(
            "fgdb-warden:1:{}:{}:{}:{}",
            hex(&namespace.0),
            hex(graph.as_bytes()),
            schema_epoch.0,
            policy_epoch,
        );
        if identifier.len() > MAX_IDENTIFIER_BYTES {
            return Err(Error::TooLarge);
        }
        Ok(Self {
            key,
            identifier,
            namespace,
            retired: AtomicBool::new(false),
        })
    }

    /// Public storage binding already authenticated by the signed identifier.
    /// This reveals no signing or attenuation key.
    #[must_use]
    pub const fn namespace(&self) -> DatabaseSecurityNamespaceId {
        self.namespace
    }

    /// Irreversibly stop this issuer incarnation, including all its borrowed
    /// capabilities and execution permits. Returns true only for the first
    /// retirement; repeated or concurrent retirement is idempotent.
    ///
    /// The trusted host calls this after ordering the governing policy change
    /// and before releasing work under the new policy. Existing operators must
    /// checkpoint before effects and result release. A checkpoint racing this
    /// call may finish first; retirement neither waits for quiescence nor undoes
    /// earlier effects. No clock, allocation, I/O, or hidden worker is involved.
    ///
    /// Restart must reconstruct only the current authoritative epoch/key. A
    /// newly constructed Authority with the OLD key and identity would still
    /// authenticate old bearer bytes; this in-memory bit is not durable state.
    pub fn retire(&self) -> bool {
        !self.retired.swap(true, Ordering::AcqRel)
    }

    /// Informational state only; callers must use the checked admission APIs.
    #[must_use]
    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    pub(crate) fn check_active(&self) -> Result<(), Error> {
        if self.is_retired() {
            Err(Error::AuthorityRetired)
        } else {
            Ok(())
        }
    }

    /// Mint all mandatory root restrictions atomically: no unrestricted
    /// intermediate macaroon is returned to the caller.
    pub fn issue_at(&self, grant: &Grant, now_ms: u64) -> Result<CapabilityToken, Error> {
        self.check_active()?;
        validate_name(&grant.branch)?;
        validate_scope(&grant.labels)?;
        validate_scope(&grant.relations)?;
        validate_scope(&grant.properties)?;
        if grant.expires_at_ms <= now_ms {
            return Err(Error::Expired);
        }
        let restrictions = [
            Restriction::Branch(grant.branch.clone()),
            Restriction::Labels(grant.labels.clone()),
            Restriction::Relations(grant.relations.clone()),
            Restriction::Properties(grant.properties.clone()),
            Restriction::Rights(grant.rights),
            Restriction::MaxNodes(grant.limits.max_nodes),
            Restriction::MaxWork(grant.limits.max_work),
            Restriction::MaxRows(grant.limits.max_rows),
            Restriction::NotBefore(now_ms),
            Restriction::ExpiresBefore(grant.expires_at_ms),
        ];
        // Validate all caller-controlled lengths before invoking foundation
        // serialization, whose u16 length checks would otherwise panic.
        let predicates = restrictions
            .iter()
            .map(encode_restriction)
            .collect::<Result<Vec<_>, _>>()?;
        let mut token = MacaroonToken::mint(&self.key, &self.identifier, LOCATION);
        for predicate in predicates {
            token = token.add_caveat(predicate);
        }
        validate_shape(&token)?;
        if token.to_binary().len() > MAX_TOKEN_BYTES {
            return Err(Error::TooLarge);
        }
        // Do not issue something that this authority cannot subsequently use.
        compile(&token)?.check_at(&grant.branch, now_ms)?;
        self.check_active()?;
        Ok(CapabilityToken { token })
    }

    /// `branch` and `now_ms` are authoritative host context, not token claims.
    /// The host must select this authority using the requested database/graph
    /// and its current catalog/policy epochs before calling this method.
    pub fn verify_at(
        &self,
        token: &CapabilityToken,
        branch: &str,
        now_ms: u64,
    ) -> Result<VerifiedCapability<'_>, Error> {
        self.check_active()?;
        if token.token.identifier() != self.identifier {
            return Err(Error::WrongAuthority);
        }
        // Cryptography is reused, never reimplemented. Generic foundation
        // Custom equality checks are NOT sufficient for object predicates;
        // Warden evaluates every admitted predicate with its own closed DSL.
        if !token.token.verify_signature(&self.key) {
            return Err(Error::Unauthenticated);
        }
        let program = compile(&token.token)?;
        program.check_at(branch, now_ms)?;
        self.check_active()?;
        Ok(VerifiedCapability {
            _authority: self,
            program,
        })
    }

    /// Recheck a preverified value received from another Rust component.
    /// A value verified by a caller-created issuer, even with an identical
    /// public identifier, is NOT evidence from this host-owned authority.
    pub fn recheck_at(
        &self,
        capability: &VerifiedCapability<'_>,
        branch: &str,
        now_ms: u64,
    ) -> Result<(), Error> {
        if !core::ptr::eq(self, capability._authority) {
            return Err(Error::WrongAuthority);
        }
        self.check_active()?;
        capability.program.check_at(branch, now_ms)
    }
}

/// Only successful authority verification constructs this type. No public
/// constructor, deserializer, Clone, or mutable policy access is provided.
pub struct VerifiedCapability<'a> {
    _authority: &'a Authority,
    program: PlannerPredicates,
}

impl fmt::Debug for VerifiedCapability<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VerifiedCapability(<redacted>)")
    }
}

impl VerifiedCapability<'_> {
    #[must_use]
    pub fn predicates(&self) -> &PlannerPredicates {
        &self.program
    }

    pub fn begin_read_at(
        &self,
        branch: &str,
        now_ms: u64,
    ) -> Result<ExecutionPermit<'_, ReadAccess>, Error> {
        self._authority.check_active()?;
        self.program.check_at(branch, now_ms)?;
        if !self.program.rights.can_read() {
            return Err(Error::PermissionDenied);
        }
        Ok(ExecutionPermit::new(&self.program, self._authority, now_ms))
    }

    /// This grants an operation allowance, not authority over arbitrary
    /// mutation targets. The write planner must ALSO check both before- and
    /// after-images, touched properties, relation and endpoint predicates.
    pub fn begin_write_at(
        &self,
        branch: &str,
        now_ms: u64,
    ) -> Result<ExecutionPermit<'_, WriteAccess>, Error> {
        self._authority.check_active()?;
        self.program.check_at(branch, now_ms)?;
        if !self.program.rights.can_write() {
            return Err(Error::PermissionDenied);
        }
        Ok(ExecutionPermit::new(&self.program, self._authority, now_ms))
    }
}

fn validate_shape(token: &MacaroonToken) -> Result<(), Error> {
    if token.identifier().len() > MAX_IDENTIFIER_BYTES
        || token.location().len() > crate::MAX_NAME_BYTES
        || token.caveat_count() > MAX_CAVEATS
    {
        return Err(Error::TooLarge);
    }
    for caveat in token.caveats() {
        let Caveat::FirstParty { predicate } = caveat else {
            return Err(Error::UnsupportedCaveat);
        };
        match predicate {
            CaveatPredicate::TimeBefore(_) | CaveatPredicate::TimeAfter(_) => {}
            CaveatPredicate::Custom(key, value) => {
                if key.len() > crate::MAX_NAME_BYTES || value.len() > MAX_VALUE_BYTES {
                    return Err(Error::TooLarge);
                }
            }
            _ => return Err(Error::UnsupportedCaveat),
        }
    }
    Ok(())
}

fn custom(key: &str, value: String) -> Result<CaveatPredicate, Error> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(Error::TooLarge);
    }
    Ok(CaveatPredicate::Custom(key.to_owned(), value))
}

fn encode_restriction(restriction: &Restriction) -> Result<CaveatPredicate, Error> {
    match restriction {
        Restriction::Branch(branch) => {
            validate_name(branch)?;
            custom(BRANCH, branch.clone())
        }
        Restriction::Labels(scope) => custom(LABELS, encode_scope(scope, |id| id.0)?),
        Restriction::Relations(scope) => custom(RELATIONS, encode_scope(scope, |id| id.0)?),
        Restriction::Properties(scope) => custom(PROPERTIES, encode_scope(scope, |id| id.0)?),
        Restriction::DenyProperties(keys) => {
            if keys.len() > MAX_SCOPE_ORDINALS {
                return Err(Error::TooLarge);
            }
            custom(
                DENY_PROPERTIES,
                encode_scope(&Scope::Only(keys.clone()), |id| id.0)?,
            )
        }
        Restriction::Rights(rights) => custom(RIGHTS, rights.bits().to_string()),
        Restriction::MaxNodes(value) => custom(MAX_NODES, value.to_string()),
        Restriction::MaxWork(value) => custom(MAX_WORK, value.to_string()),
        Restriction::MaxRows(value) => custom(MAX_ROWS, value.to_string()),
        Restriction::ExpiresBefore(value) => Ok(CaveatPredicate::TimeBefore(*value)),
        Restriction::NotBefore(value) => Ok(CaveatPredicate::TimeAfter(*value)),
    }
}

fn encode_scope<T: Ord>(scope: &Scope<T>, ordinal: impl Fn(&T) -> u64) -> Result<String, Error> {
    validate_scope(scope)?;
    let Scope::Only(values) = scope else {
        return Ok("*".to_owned());
    };
    if values.len() > MAX_SCOPE_ORDINALS {
        return Err(Error::TooLarge);
    }
    if values.is_empty() {
        return Ok("-".to_owned());
    }
    Ok(values
        .iter()
        .map(|value| format!("{:016x}", ordinal(value)))
        .collect::<Vec<_>>()
        .join(","))
}

fn validate_scope<T: Ord>(scope: &Scope<T>) -> Result<(), Error> {
    if let Scope::Only(values) = scope {
        if values.len() > MAX_SCOPE_ORDINALS {
            return Err(Error::TooLarge);
        }
    }
    Ok(())
}

fn parse_scope<T: Ord>(value: &str, wrap: impl Fn(u64) -> T) -> Result<Scope<T>, Error> {
    if value == "*" {
        return Ok(Scope::All);
    }
    if value == "-" {
        return Ok(Scope::Only(BTreeSet::new()));
    }
    let mut values = BTreeSet::new();
    let mut previous = None;
    for item in value.split(',') {
        if item.len() != 16
            || !item
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Malformed);
        }
        let ordinal = u64::from_str_radix(item, 16).map_err(|_| Error::Malformed)?;
        if previous.is_some_and(|prev| prev >= ordinal) {
            return Err(Error::Malformed);
        }
        if values.len() >= MAX_SCOPE_ORDINALS {
            return Err(Error::TooLarge);
        }
        previous = Some(ordinal);
        values.insert(wrap(ordinal));
    }
    Ok(Scope::Only(values))
}

fn unsigned(value: &str) -> Result<u64, Error> {
    if value.is_empty()
        || value.len() > 20
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(Error::Malformed);
    }
    value.parse().map_err(|_| Error::Malformed)
}

fn decode_restriction(predicate: &CaveatPredicate) -> Result<Restriction, Error> {
    match predicate {
        CaveatPredicate::TimeBefore(time) => Ok(Restriction::ExpiresBefore(*time)),
        CaveatPredicate::TimeAfter(time) => Ok(Restriction::NotBefore(*time)),
        CaveatPredicate::Custom(key, value) => match key.as_str() {
            BRANCH => {
                validate_name(value)?;
                Ok(Restriction::Branch(value.clone()))
            }
            LABELS => Ok(Restriction::Labels(parse_scope(value, LabelId)?)),
            RELATIONS => Ok(Restriction::Relations(parse_scope(value, RelationId)?)),
            PROPERTIES => Ok(Restriction::Properties(parse_scope(value, PropertyKeyId)?)),
            DENY_PROPERTIES => match parse_scope(value, PropertyKeyId)? {
                Scope::Only(keys) => Ok(Restriction::DenyProperties(keys)),
                Scope::All => Err(Error::Malformed),
            },
            RIGHTS => Ok(Restriction::Rights(Rights::from_bits(unsigned(value)?)?)),
            MAX_NODES => Ok(Restriction::MaxNodes(unsigned(value)?)),
            MAX_WORK => Ok(Restriction::MaxWork(unsigned(value)?)),
            MAX_ROWS => Ok(Restriction::MaxRows(unsigned(value)?)),
            _ => Err(Error::UnsupportedCaveat),
        },
        _ => Err(Error::UnsupportedCaveat),
    }
}

fn compile(token: &MacaroonToken) -> Result<PlannerPredicates, Error> {
    validate_shape(token)?;
    // Initial universe values NEVER escape this function until all ten
    // required root restrictions have been encountered and compiled.
    let mut seen: u16 = 0;
    let mut branch: Option<String> = None;
    let mut program = PlannerPredicates {
        branch: String::new(),
        label_clauses: Vec::new(),
        relations: Scope::All,
        properties: Scope::All,
        denied_properties: BTreeSet::new(),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: u64::MAX,
            max_work: u64::MAX,
            max_rows: u64::MAX,
        },
        not_before_ms: 0,
        expires_at_ms: u64::MAX,
    };
    for caveat in token.caveats() {
        let Caveat::FirstParty { predicate } = caveat else {
            return Err(Error::UnsupportedCaveat);
        };
        match decode_restriction(predicate)? {
            Restriction::Branch(next) => {
                seen |= 1 << 0;
                if branch.as_ref().is_some_and(|previous| previous != &next) {
                    return Err(Error::ScopeDenied);
                }
                branch = Some(next);
            }
            Restriction::Labels(scope) => {
                seen |= 1 << 1;
                if let Scope::Only(labels) = scope {
                    program.label_clauses.push(labels);
                }
            }
            Restriction::Relations(scope) => {
                seen |= 1 << 2;
                program.relations.intersect(scope);
            }
            Restriction::Properties(scope) => {
                seen |= 1 << 3;
                program.properties.intersect(scope);
            }
            Restriction::Rights(rights) => {
                seen |= 1 << 4;
                program.rights = program.rights.intersect(rights);
            }
            Restriction::MaxNodes(limit) => {
                seen |= 1 << 5;
                program.limits.max_nodes = program.limits.max_nodes.min(limit);
            }
            Restriction::MaxWork(limit) => {
                seen |= 1 << 6;
                program.limits.max_work = program.limits.max_work.min(limit);
            }
            Restriction::MaxRows(limit) => {
                seen |= 1 << 7;
                program.limits.max_rows = program.limits.max_rows.min(limit);
            }
            Restriction::NotBefore(time) => {
                seen |= 1 << 8;
                program.not_before_ms = program.not_before_ms.max(time);
            }
            Restriction::ExpiresBefore(time) => {
                seen |= 1 << 9;
                program.expires_at_ms = program.expires_at_ms.min(time);
            }
            Restriction::DenyProperties(keys) => program.denied_properties.extend(keys),
        }
    }
    if seen != (1 << 10) - 1 {
        return Err(Error::MissingRestriction);
    }
    program.branch = branch.ok_or(Error::MissingRestriction)?;
    program.label_clauses.sort();
    program.label_clauses.dedup();
    Ok(program)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    result
}
