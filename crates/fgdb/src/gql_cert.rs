//! Deterministic certificates for the bounded GQL execution slice.
//!
//! `GqlCertificate` binds the exact statement bytes, canonical relation bind,
//! and MVCC snapshot used by the public execution API. `GqlPlanCertificate`
//! separately binds the executor-ready `BoundPlan` and snapshot. Exact ordered
//! result rows are bound by a domain-separated digest derived from the plan
//! certificate; no type here claims to attest runtime cost or an operator tree.

use crate::{Database, EmbeddedReadView, GqlError};
use asupersync::fs::Vfs;
use fgdb_crypto::{Digest, Hasher, hash};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{BoundPlan, EdgeDirection, RelationBind, ReturnProjection};
use fgdb_types::{CommitSeq, VId};

const GQL_PLAN_CERTIFICATE_DOMAIN_V1: &[u8] = b"fgdb:gql-bound-plan-certificate:v1";
const GQL_PLAN_CERTIFICATE_DOMAIN_V2: &[u8] = b"fgdb:gql-bound-plan-certificate:v2";
const GQL_RESULT_DIGEST_DOMAIN_V1: &[u8] = b"fgdb:gql-ordered-result-digest:v1";

/// Native read grammar domains. Tags are part of the v1 certificate transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NativeReadClass {
    Pattern = 0,
    Aggregate = 1,
    PipelineAggregate = 2,
    Set = 3,
    TemporalPattern = 4,
    TemporalSet = 5,
    TemporalAggregate = 6,
}

impl TryFrom<u8> for NativeReadClass {
    type Error = ();
    fn try_from(tag: u8) -> Result<Self, Self::Error> {
        match tag {
            0 => Ok(Self::Pattern),
            1 => Ok(Self::Aggregate),
            2 => Ok(Self::PipelineAggregate),
            3 => Ok(Self::Set),
            4 => Ok(Self::TemporalPattern),
            5 => Ok(Self::TemporalSet),
            6 => Ok(Self::TemporalAggregate),
            _ => Err(()),
        }
    }
}

/// Resolved, unbound native plan evidence. Implementations must retain literal
/// constants and parameter identities, but must not encode argument values.
pub trait NativeCertificatePlan {
    fn facade_class(&self) -> NativeReadClass;
    fn canonical_bytes(&self) -> Vec<u8>;
    fn parameter_schema(&self) -> &[fgdb_gql::GqlParameterSpec];
}

const NATIVE_RESULT_CERTIFICATE_DOMAIN_V1: &[u8] = b"fgdb:native-result-certificate:v1";
const NATIVE_RESULT_DIGEST_DOMAIN_V1: &[u8] = b"fgdb:native-ordered-result-digest:v1";
const NATIVE_VALUES_DIGEST_DOMAIN_V1: &[u8] = b"fgdb:native-parameter-values:v1";

/// Identity evidence only: neither result correctness nor replay execution.
/// The digest identifies the template independently of argument values and
/// snapshot selection; `verifies_at` separately checks the selected frontier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativePlanCertificate {
    pub digest: Digest,
    pub snapshot_seq: CommitSeq,
}

impl NativePlanCertificate {
    #[must_use]
    pub fn new(prepared: &impl NativeCertificatePlan, snapshot_seq: CommitSeq) -> Self {
        Self {
            digest: native_plan_digest(prepared),
            snapshot_seq,
        }
    }

    #[must_use]
    pub fn verifies(&self, prepared: &impl NativeCertificatePlan) -> bool {
        digest_eq(self.digest, native_plan_digest(prepared))
    }

    #[must_use]
    pub fn verifies_at(
        &self,
        prepared: &impl NativeCertificatePlan,
        snapshot_seq: CommitSeq,
    ) -> bool {
        self.snapshot_seq == snapshot_seq && self.verifies(prepared)
    }

    /// Fixed, versioned certificate envelope, including the snapshot binding.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:native-plan-certificate:v1\0".to_vec();
        bytes.extend_from_slice(&self.digest.0);
        bytes.extend_from_slice(&self.snapshot_seq.0.to_be_bytes());
        bytes
    }
}

fn native_plan_digest(prepared: &impl NativeCertificatePlan) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(b"fgdb:native-read-template:v1\0");
    hasher.update(&[prepared.facade_class() as u8]);
    let bytes = prepared.canonical_bytes();
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(&bytes);
    let schema = prepared.parameter_schema();
    hasher.update(&(schema.len() as u64).to_be_bytes());
    for spec in schema {
        update_string(&mut hasher, &spec.name);
        match spec.parameter_type {
            fgdb_gql::GqlParameterType::Int64 => {
                hasher.update(&[0]);
            }
            fgdb_gql::GqlParameterType::UInt64 => {
                hasher.update(&[1]);
            }
            fgdb_gql::GqlParameterType::Scalar(kind) => {
                hasher.update(&[2, kind as u8]);
            }
            fgdb_gql::GqlParameterType::List => {
                hasher.update(&[3]);
            }
        }
        hasher.update(&[u8::from(spec.requires_positive)]);
        hasher.update(&(spec.occurrences as u64).to_be_bytes());
    }
    hasher.finalize()
}
/// Replay evidence for the public statement execution surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlCertificate {
    pub snapshot_seq: CommitSeq,
    pub statement_digest: Digest,
    pub bind_digest: Digest,
}

impl GqlCertificate {
    /// Verify the statement and bind portions against this certificate's own
    /// snapshot declaration.
    ///
    /// This deliberately does not execute the query and does not claim that
    /// result rows are certified. It proves only that the supplied statement
    /// bytes and canonical bind map are the inputs this value names.
    #[must_use]
    pub fn verifies(&self, statement: &str, bind: &RelationBind) -> bool {
        digest_eq(self.statement_digest, digest_statement(statement))
            && digest_eq(self.bind_digest, digest_bind(bind))
    }

    /// Verify the complete public certificate tuple, including the explicitly
    /// expected MVCC snapshot.
    #[must_use]
    pub fn verifies_at(
        &self,
        statement: &str,
        bind: &RelationBind,
        snapshot_seq: CommitSeq,
    ) -> bool {
        self.snapshot_seq == snapshot_seq && self.verifies(statement, bind)
    }
}

/// A replay-stable identity for a bound GQL plan at one snapshot frontier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlPlanCertificate {
    pub digest: Digest,
    pub snapshot_seq: CommitSeq,
}

impl GqlPlanCertificate {
    /// Verify this certificate against `plan` and the certificate's declared
    /// snapshot using the current v2 transcript.
    #[must_use]
    pub fn verifies(&self, plan: &BoundPlan) -> bool {
        let expected = certify(plan, self.snapshot_seq);
        self.snapshot_seq == expected.snapshot_seq && digest_eq(self.digest, expected.digest)
    }

    /// Verify this certificate against an explicitly expected snapshot.
    #[must_use]
    pub fn verifies_at(&self, plan: &BoundPlan, snapshot_seq: CommitSeq) -> bool {
        self.snapshot_seq == snapshot_seq && self.verifies(plan)
    }

    /// Verify a certificate produced by the historical v1 transcript.
    ///
    /// V1 predates `BoundPlan::neq` and therefore does not bind that field.
    /// This method exists only to make migration explicit; new certificates
    /// are always produced by [`certify`] under v2.
    #[must_use]
    pub fn verifies_v1_legacy(&self, plan: &BoundPlan) -> bool {
        let expected = certify_v1_legacy(plan, self.snapshot_seq);
        self.snapshot_seq == expected.snapshot_seq && digest_eq(self.digest, expected.digest)
    }

    /// Bind one exact ordered result to this plan certificate and snapshot.
    ///
    /// The transcript contains this certificate's digest, its snapshot, the
    /// exact row count, and every returned vertex identifier in order. It is
    /// deliberately a digest layer rather than a portable artifact format.
    #[must_use]
    pub fn result_digest(&self, rows: &[VId]) -> Digest {
        digest_result(self, rows)
    }

    /// Verify one exact ordered result digest in constant work over the final
    /// digest comparison.
    #[must_use]
    pub fn verifies_result_digest(&self, rows: &[VId], result_digest: Digest) -> bool {
        digest_eq(self.result_digest(rows), result_digest)
    }
}

/// Certify every field of the executor-ready plan and its MVCC snapshot.
///
/// V2 adds `BoundPlan::neq`, the one current plan field omitted by the original
/// transcript. The domain changed with the transcript, so no v1 certificate can
/// be misread as v2 even when every other field is identical.
pub fn certify(plan: &BoundPlan, snapshot_seq: CommitSeq) -> GqlPlanCertificate {
    certify_with_domain(plan, snapshot_seq, GQL_PLAN_CERTIFICATE_DOMAIN_V2, true)
}

fn execute_with_certificates_at<R: crate::gql_exec::GqlSnapshotReader + ?Sized>(
    reader: &R,
    statement: &str,
    bind: &RelationBind,
    plan: &BoundPlan,
    as_of: CommitSeq,
) -> Result<(Vec<VId>, GqlCertificate, GqlPlanCertificate), GqlError> {
    let rows = crate::gql_exec::execute_at(plan, reader, as_of).map_err(GqlError::Read)?;
    let input_certificate = GqlCertificate {
        snapshot_seq: as_of,
        statement_digest: digest_statement(statement),
        bind_digest: digest_bind(bind),
    };
    let plan_certificate = certify(plan, as_of);
    Ok((rows, input_certificate, plan_certificate))
}

fn execute_with_result_digest_at<R: crate::gql_exec::GqlSnapshotReader + ?Sized>(
    reader: &R,
    statement: &str,
    bind: &RelationBind,
    plan: &BoundPlan,
    as_of: CommitSeq,
) -> Result<(Vec<VId>, GqlCertificate, GqlPlanCertificate, Digest), GqlError> {
    let (rows, input_certificate, plan_certificate) =
        execute_with_certificates_at(reader, statement, bind, plan, as_of)?;
    let result_digest = plan_certificate.result_digest(&rows);
    Ok((rows, input_certificate, plan_certificate, result_digest))
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute once and return both existing certificate layers aligned to
    /// the same live frontier.
    ///
    /// Parsing and binding happen once, the shared snapshot kernel executes
    /// once, and evidence is minted only after that read succeeds. The
    /// [`GqlCertificate`] binds statement bytes and canonical bind input; the
    /// [`GqlPlanCertificate`] binds the complete executor-ready plan. Neither
    /// certificate attests the returned rows.
    pub fn execute_gql_with_certificates(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<(Vec<VId>, GqlCertificate, GqlPlanCertificate), GqlError> {
        let plan = self.prepare_gql_plan(statement, bind)?;
        let as_of = self.frontier().map_err(GqlError::Read)?;
        execute_with_certificates_at(self, statement, bind, &plan, as_of)
    }

    /// Execute once at `as_of` and return input and plan certificates naming
    /// that exact successful read.
    ///
    /// A future, fenced, or otherwise refused read returns its existing typed
    /// error and no evidence tuple.
    pub fn execute_gql_with_certificates_at(
        &self,
        statement: &str,
        bind: &RelationBind,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, GqlCertificate, GqlPlanCertificate), GqlError> {
        let plan = self.prepare_gql_plan(statement, bind)?;
        execute_with_certificates_at(self, statement, bind, &plan, as_of)
    }

    /// Execute once and return input, plan, and exact ordered-result evidence
    /// aligned to the same live frontier.
    pub fn execute_gql_with_result_digest(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<(Vec<VId>, GqlCertificate, GqlPlanCertificate, Digest), GqlError> {
        let plan = self.prepare_gql_plan(statement, bind)?;
        let as_of = self.frontier().map_err(GqlError::Read)?;
        execute_with_result_digest_at(self, statement, bind, &plan, as_of)
    }

    /// Execute once at `as_of` and bind the exact ordered rows to the plan
    /// certificate minted for that same successful read.
    pub fn execute_gql_with_result_digest_at(
        &self,
        statement: &str,
        bind: &RelationBind,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, GqlCertificate, GqlPlanCertificate, Digest), GqlError> {
        let plan = self.prepare_gql_plan(statement, bind)?;
        execute_with_result_digest_at(self, statement, bind, &plan, as_of)
    }

    /// Execute one already-bound plan at the live frontier and bind its exact
    /// ordered rows to the plan certificate returned by the same execution.
    pub fn execute_prepared_gql_with_result_digest(
        &self,
        plan: &BoundPlan,
    ) -> Result<(Vec<VId>, GqlPlanCertificate, Digest), GqlError> {
        let (rows, plan_certificate) = self.execute_prepared_gql_certified(plan)?;
        let result_digest = plan_certificate.result_digest(&rows);
        Ok((rows, plan_certificate, result_digest))
    }

    /// Execute one already-bound plan at `as_of` and bind its exact ordered
    /// rows to the plan certificate returned by that historical execution.
    pub fn execute_prepared_gql_with_result_digest_at(
        &self,
        plan: &BoundPlan,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, GqlPlanCertificate, Digest), GqlError> {
        let (rows, plan_certificate) = self.execute_prepared_gql_certified_at(plan, as_of)?;
        let result_digest = plan_certificate.result_digest(&rows);
        Ok((rows, plan_certificate, result_digest))
    }
}

impl EmbeddedReadView {
    /// Execute once and return both certificate layers aligned to this view's
    /// pinned frontier.
    pub fn execute_gql_with_certificates(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<(Vec<VId>, GqlCertificate, GqlPlanCertificate), GqlError> {
        self.execute_gql_with_certificates_at(statement, bind, self.frontier())
    }

    /// Execute once at a retained sequence and return both certificate layers
    /// naming that exact successful read.
    pub fn execute_gql_with_certificates_at(
        &self,
        statement: &str,
        bind: &RelationBind,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, GqlCertificate, GqlPlanCertificate), GqlError> {
        let plan = self.prepare_gql_plan(statement, bind)?;
        execute_with_certificates_at(self, statement, bind, &plan, as_of)
    }

    /// Execute once and return input, plan, and exact ordered-result evidence
    /// aligned to this view's pinned frontier.
    pub fn execute_gql_with_result_digest(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<(Vec<VId>, GqlCertificate, GqlPlanCertificate, Digest), GqlError> {
        self.execute_gql_with_result_digest_at(statement, bind, self.frontier())
    }

    /// Execute once at a retained sequence and bind the exact ordered rows to
    /// the plan certificate minted for that same read.
    pub fn execute_gql_with_result_digest_at(
        &self,
        statement: &str,
        bind: &RelationBind,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, GqlCertificate, GqlPlanCertificate, Digest), GqlError> {
        let plan = self.prepare_gql_plan(statement, bind)?;
        execute_with_result_digest_at(self, statement, bind, &plan, as_of)
    }

    /// Execute one already-bound plan at this view's pinned frontier and bind
    /// its exact ordered rows to the plan certificate from that execution.
    pub fn execute_prepared_gql_with_result_digest(
        &self,
        plan: &BoundPlan,
    ) -> Result<(Vec<VId>, GqlPlanCertificate, Digest), GqlError> {
        let (rows, plan_certificate) = self.execute_prepared_gql_certified(plan)?;
        let result_digest = plan_certificate.result_digest(&rows);
        Ok((rows, plan_certificate, result_digest))
    }

    /// Execute one already-bound plan at a retained sequence and bind its
    /// exact ordered rows to the same historical plan certificate.
    pub fn execute_prepared_gql_with_result_digest_at(
        &self,
        plan: &BoundPlan,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, GqlPlanCertificate, Digest), GqlError> {
        let (rows, plan_certificate) = self.execute_prepared_gql_certified_at(plan, as_of)?;
        let result_digest = plan_certificate.result_digest(&rows);
        Ok((rows, plan_certificate, result_digest))
    }
}

/// Recompute the historical v1 transcript for explicit migration checks.
fn certify_v1_legacy(plan: &BoundPlan, snapshot_seq: CommitSeq) -> GqlPlanCertificate {
    certify_with_domain(plan, snapshot_seq, GQL_PLAN_CERTIFICATE_DOMAIN_V1, false)
}

fn certify_with_domain(
    plan: &BoundPlan,
    snapshot_seq: CommitSeq,
    domain: &[u8],
    include_neq: bool,
) -> GqlPlanCertificate {
    let mut hasher = Hasher::new();
    hasher.update(domain);
    update_relation(&mut hasher, plan.relation);
    update_string(&mut hasher, &plan.src_var);
    update_string(&mut hasher, &plan.dst_var);
    update_string(&mut hasher, &plan.via_var);
    update_relation(&mut hasher, plan.hop2_relation);
    update_optional_string(&mut hasher, plan.hop2_dst_var.as_deref());
    hasher.update(&[projection_tag(plan.projection)]);
    hasher.update(&[direction_tag(plan.direction)]);
    update_label(&mut hasher, plan.src_label);
    update_label(&mut hasher, plan.dst_label);
    update_string_pair(&mut hasher, plan.eq.as_ref());
    if include_neq {
        update_string_pair(&mut hasher, plan.neq.as_ref());
    }
    update_property(&mut hasher, plan.src_prop);
    update_property(&mut hasher, plan.src_prop_ne);
    update_property(&mut hasher, plan.src_prop_gt);
    update_property(&mut hasher, plan.src_prop_lt);
    update_property(&mut hasher, plan.src_prop_ge);
    update_property(&mut hasher, plan.src_prop_le);
    update_property(&mut hasher, plan.dst_prop);
    update_property(&mut hasher, plan.dst_prop_ne);
    update_property(&mut hasher, plan.dst_prop_gt);
    update_property(&mut hasher, plan.dst_prop_lt);
    update_property(&mut hasher, plan.dst_prop_ge);
    update_property(&mut hasher, plan.dst_prop_le);
    update_property(&mut hasher, plan.hop2_dst_prop);
    update_property(&mut hasher, plan.hop2_dst_prop_ne);
    update_property(&mut hasher, plan.hop2_dst_prop_gt);
    update_property(&mut hasher, plan.hop2_dst_prop_lt);
    update_property(&mut hasher, plan.hop2_dst_prop_ge);
    update_property(&mut hasher, plan.hop2_dst_prop_le);
    update_optional_u64(&mut hasher, plan.limit);
    update_optional_u64(&mut hasher, plan.skip);
    hasher.update(&snapshot_seq.0.to_be_bytes());

    GqlPlanCertificate {
        digest: hasher.finalize(),
        snapshot_seq,
    }
}

fn digest_result(plan_certificate: &GqlPlanCertificate, rows: &[VId]) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(GQL_RESULT_DIGEST_DOMAIN_V1);
    hasher.update(&plan_certificate.digest.0);
    hasher.update(&plan_certificate.snapshot_seq.0.to_be_bytes());
    hasher.update(&(rows.len() as u64).to_be_bytes());
    for row in rows {
        hasher.update(&row.0.to_be_bytes());
    }
    hasher.finalize()
}

fn digest_eq(left: Digest, right: Digest) -> bool {
    left.0
        .iter()
        .zip(right.0.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (*left ^ *right)
        })
        == 0
}

fn update_relation(hasher: &mut Hasher, relation: Option<RelationId>) {
    match relation {
        None => {
            hasher.update(&[0]);
        }
        Some(relation) => {
            hasher.update(&[1]);
            hasher.update(&relation.0.to_be_bytes());
        }
    }
}

fn update_label(hasher: &mut Hasher, label: Option<LabelId>) {
    match label {
        None => {
            hasher.update(&[0]);
        }
        Some(label) => {
            hasher.update(&[1]);
            hasher.update(&label.0.to_be_bytes());
        }
    }
}

fn update_optional_string(hasher: &mut Hasher, value: Option<&str>) {
    match value {
        None => {
            hasher.update(&[0]);
        }
        Some(value) => {
            hasher.update(&[1]);
            update_string(hasher, value);
        }
    }
}

fn update_string_pair(hasher: &mut Hasher, pair: Option<&(String, String)>) {
    match pair {
        None => {
            hasher.update(&[0]);
        }
        Some((left, right)) => {
            hasher.update(&[1]);
            update_string(hasher, left);
            update_string(hasher, right);
        }
    }
}

fn update_property(hasher: &mut Hasher, property: Option<(PropertyKeyId, i64)>) {
    match property {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
}

fn update_optional_u64(hasher: &mut Hasher, value: Option<u64>) {
    match value {
        None => {
            hasher.update(&[0]);
        }
        Some(value) => {
            hasher.update(&[1]);
            hasher.update(&value.to_be_bytes());
        }
    }
}

fn direction_tag(direction: EdgeDirection) -> u8 {
    match direction {
        EdgeDirection::Outgoing => 0,
        EdgeDirection::Incoming => 1,
        EdgeDirection::Undirected => 2,
    }
}

fn projection_tag(projection: ReturnProjection) -> u8 {
    match projection {
        ReturnProjection::Destination => 0,
        ReturnProjection::Source => 1,
        ReturnProjection::Hop2Destination => 2,
    }
}

fn update_string(hasher: &mut Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

pub fn digest_statement(src: &str) -> Digest {
    hash(src.as_bytes())
}

pub fn digest_bind(bind: &RelationBind) -> Digest {
    hash(&bind.canonical_bytes())
}

/// One certified native read result: the plan certificate it executed under,
/// the parameter values it was bound with, and the exact ordered result bytes.
///
/// This is the replay unit for FG-INV-19's local grade: certificate + these
/// bindings re-executed at the certified snapshot must be byte-identical.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeResultCertificate {
    pub plan: NativePlanCertificate,
    pub values_digest: Digest,
    pub result_digest: Digest,
    pub statement: String,
    pub facade_class: NativeReadClass,
    pub snapshot_identity: Digest,
    pub database_identity: Digest,
}

impl NativeResultCertificate {
    /// Portable v1 envelope: plan identity and snapshot, parameter and result
    /// digests, history/database identities, facade class, and statement text.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = NATIVE_RESULT_CERTIFICATE_DOMAIN_V1.to_vec();
        bytes.push(NATIVE_RESULT_CERTIFICATE_VERSION_V1);
        bytes.extend_from_slice(&self.plan.digest.0);
        bytes.extend_from_slice(&self.plan.snapshot_seq.0.to_be_bytes());
        bytes.extend_from_slice(&self.values_digest.0);
        bytes.extend_from_slice(&self.result_digest.0);
        bytes.extend_from_slice(&self.snapshot_identity.0);
        bytes.extend_from_slice(&self.database_identity.0);
        bytes.push(self.facade_class as u8);
        bytes.extend_from_slice(&(self.statement.len() as u64).to_be_bytes());
        bytes.extend_from_slice(self.statement.as_bytes());
        bytes
    }
}

const NATIVE_RESULT_CERTIFICATE_VERSION_V1: u8 = 1;

/// Decode error for the versioned portable certificate envelope. Every
/// refusal is explicit: version, length, magic, or trailing bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertificateDecodeError {
    /// Fewer bytes than the fixed header requires.
    Truncated,
    /// Leading domain magic differs.
    Magic,
    /// Unknown envelope version byte.
    Version(u8),
    /// Statement length prefix exceeds the remaining bytes or is not UTF-8.
    Statement,
    /// Bytes remain after the complete envelope.
    Trailing(usize),
    /// Facade class tag is outside the admitted set.
    FacadeClass(u8),
}
impl core::fmt::Display for CertificateDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "certificate truncated"),
            Self::Magic => write!(f, "certificate magic mismatch"),
            Self::Version(v) => write!(f, "certificate version {v} unsupported"),
            Self::Statement => write!(f, "certificate statement length invalid"),
            Self::Trailing(n) => write!(f, "{n} trailing certificate bytes"),
            Self::FacadeClass(tag) => write!(f, "certificate facade class {tag} unknown"),
        }
    }
}
impl core::error::Error for CertificateDecodeError {}

impl NativeResultCertificate {
    /// Strict decode of the v1 envelope: exact magic, version, fixed fields,
    /// statement length prefix, and zero trailing bytes.
    ///
    /// # Errors
    /// Typed [`CertificateDecodeError`] on any structural mismatch.
    pub fn decode(bytes: &[u8]) -> Result<Self, CertificateDecodeError> {
        let magic = NATIVE_RESULT_CERTIFICATE_DOMAIN_V1.len();
        let fixed = magic + 1 + 5 * 32 + 8 + 1 + 8;
        if bytes.len() < fixed {
            return Err(CertificateDecodeError::Truncated);
        }
        if &bytes[..magic] != NATIVE_RESULT_CERTIFICATE_DOMAIN_V1 {
            return Err(CertificateDecodeError::Magic);
        }
        let mut at = magic;
        let version = bytes[at];
        at += 1;
        if version != NATIVE_RESULT_CERTIFICATE_VERSION_V1 {
            return Err(CertificateDecodeError::Version(version));
        }
        let read_digest = |at: usize| -> [u8; 32] {
            let mut digest = [0_u8; 32];
            digest.copy_from_slice(&bytes[at..at + 32]);
            digest
        };
        let plan_digest = Digest(read_digest(at));
        at += 32;
        let snapshot_seq = CommitSeq(u64::from_be_bytes(
            bytes[at..at + 8].try_into().expect("checked length"),
        ));
        at += 8;
        let values_digest = Digest(read_digest(at));
        at += 32;
        let result_digest = Digest(read_digest(at));
        at += 32;
        let snapshot_identity = Digest(read_digest(at));
        at += 32;
        let database_identity = Digest(read_digest(at));
        at += 32;
        let facade_tag = bytes[at];
        at += 1;
        let facade_class = match NativeReadClass::try_from(facade_tag) {
            Ok(class) => class,
            Err(_) => return Err(CertificateDecodeError::FacadeClass(facade_tag)),
        };
        let statement_len = usize::try_from(u64::from_be_bytes(
            bytes[at..at + 8].try_into().expect("checked length"),
        ))
        .map_err(|_| CertificateDecodeError::Statement)?;
        at += 8;
        let Some(rest) = bytes.get(at..) else {
            return Err(CertificateDecodeError::Truncated);
        };
        if rest.len() < statement_len {
            return Err(CertificateDecodeError::Truncated);
        }
        let statement = core::str::from_utf8(&rest[..statement_len])
            .map_err(|_| CertificateDecodeError::Statement)?;
        at += statement_len;
        if bytes.len() != at {
            return Err(CertificateDecodeError::Trailing(bytes.len() - at));
        }
        Ok(Self {
            plan: NativePlanCertificate {
                digest: plan_digest,
                snapshot_seq,
            },
            values_digest,
            result_digest,
            statement: statement.to_owned(),
            facade_class,
            snapshot_identity,
            database_identity,
        })
    }
}

pub(crate) fn native_values_digest(values: &fgdb_gql::GqlParameters) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(NATIVE_VALUES_DIGEST_DOMAIN_V1);
    hasher.update(&values.canonical_bytes());
    hasher.finalize()
}

/// Canonical ordered result transcript for native read rows. Cells carry
/// variant tags so Count/Integer/Average/Value never collide; GraphValue
/// cells reuse the admitted scalar/row canonical encoding.
pub(crate) fn native_result_digest(
    plan: &NativePlanCertificate,
    values: &fgdb_gql::GqlParameters,
    columns: &[String],
    rows: &[Vec<crate::QueryValue>],
) -> Result<Digest, fgdb_types::ScalarEncodeError> {
    let mut hasher = Hasher::new();
    hasher.update(NATIVE_RESULT_DIGEST_DOMAIN_V1);
    hasher.update(&plan.digest.0);
    hasher.update(&plan.snapshot_seq.0.to_be_bytes());
    hasher.update(&values.canonical_bytes());
    hasher.update(&(columns.len() as u64).to_be_bytes());
    for column in columns {
        hasher.update(&(column.len() as u64).to_be_bytes());
        hasher.update(column.as_bytes());
    }
    hasher.update(&(rows.len() as u64).to_be_bytes());
    for row in rows {
        hasher.update(&(row.len() as u64).to_be_bytes());
        for cell in row {
            match cell {
                crate::QueryValue::Count(count) => {
                    hasher.update(&[0]);
                    hasher.update(&count.to_be_bytes());
                }
                crate::QueryValue::Integer(value) => {
                    hasher.update(&[1]);
                    hasher.update(&value.to_be_bytes());
                }
                crate::QueryValue::Average(average) => {
                    hasher.update(&[2]);
                    hasher.update(&average.numerator().to_be_bytes());
                    hasher.update(&average.denominator().to_be_bytes());
                }
                crate::QueryValue::Value(value) => {
                    hasher.update(&[3]);
                    let encoded = value.canonical_bytes()?;
                    hasher.update(&(encoded.len() as u64).to_be_bytes());
                    hasher.update(&encoded);
                }
            }
        }
    }
    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::NativeResultCertificate;
    use super::{
        GqlCertificate, GqlPlanCertificate, certify, certify_v1_legacy, digest_bind,
        digest_statement, direction_tag, projection_tag,
    };
    use super::{NativeCertificatePlan, NativePlanCertificate, NativeReadClass};
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
    use fgdb_gql::{BoundPlan, EdgeDirection, RelationBind, ReturnProjection};
    use fgdb_types::{CommitSeq, VId};

    fn plan(relation: u64) -> BoundPlan {
        BoundPlan {
            relation: Some(RelationId(relation)),
            src_var: "a".to_owned(),
            dst_var: "b".to_owned(),
            src_label: None,
            dst_label: None,
            via_var: String::new(),
            hop2_relation: None,
            hop2_dst_var: None,
            projection: ReturnProjection::Destination,
            direction: EdgeDirection::Outgoing,
            neq: None,
            eq: None,
            src_prop: None,
            src_prop_ne: None,
            src_prop_gt: None,
            src_prop_lt: None,
            src_prop_ge: None,
            src_prop_le: None,
            dst_prop: None,
            dst_prop_ne: None,
            dst_prop_gt: None,
            dst_prop_lt: None,
            dst_prop_ge: None,
            dst_prop_le: None,
            limit: None,
            skip: None,
            hop2_dst_prop: None,
            hop2_dst_prop_ne: None,
            hop2_dst_prop_gt: None,
            hop2_dst_prop_lt: None,
            hop2_dst_prop_ge: None,
            hop2_dst_prop_le: None,
        }
    }

    fn assert_plan_changed(field: &str, base: &BoundPlan, changed: BoundPlan) {
        assert_ne!(
            certify(base, CommitSeq(11)).digest,
            certify(&changed, CommitSeq(11)).digest,
            "mutating {field} must change the v2 plan certificate"
        );
    }

    #[test]
    fn same_plan_and_snapshot_have_equal_certificates() {
        let plan = plan(7);
        let first = certify(&plan, CommitSeq(11));
        let second = certify(&plan, CommitSeq(11));
        assert_eq!(first, second);
        assert!(first.verifies(&plan));
        let _: GqlPlanCertificate = first;
    }

    #[test]
    fn explicit_snapshot_verification_refuses_mismatch() {
        let plan = plan(7);
        let certificate = certify(&plan, CommitSeq(11));
        assert!(certificate.verifies_at(&plan, CommitSeq(11)));
        assert!(!certificate.verifies_at(&plan, CommitSeq(12)));
    }

    #[test]
    #[allow(clippy::vec_init_then_push)]
    fn v2_transcript_binds_every_current_bound_plan_field() {
        let base = plan(7);
        let mut variants = Vec::<(&str, BoundPlan)>::new();
        macro_rules! variant {
            ($field:ident, $value:expr) => {{
                let mut changed = base.clone();
                changed.$field = $value;
                variants.push((stringify!($field), changed));
            }};
        }

        variant!(relation, Some(RelationId(8)));
        variant!(src_var, "source".to_owned());
        variant!(dst_var, "destination".to_owned());
        variant!(src_label, Some(LabelId(1)));
        variant!(dst_label, Some(LabelId(2)));
        variant!(via_var, "via".to_owned());
        variant!(hop2_relation, Some(RelationId(9)));
        variant!(hop2_dst_var, Some("far".to_owned()));
        variant!(projection, ReturnProjection::Source);
        variant!(direction, EdgeDirection::Incoming);
        variant!(neq, Some(("a".to_owned(), "b".to_owned())));
        variant!(eq, Some(("a".to_owned(), "b".to_owned())));
        variant!(src_prop, Some((PropertyKeyId(1), 1)));
        variant!(src_prop_ne, Some((PropertyKeyId(2), 2)));
        variant!(src_prop_gt, Some((PropertyKeyId(3), 3)));
        variant!(src_prop_lt, Some((PropertyKeyId(4), 4)));
        variant!(src_prop_ge, Some((PropertyKeyId(5), 5)));
        variant!(src_prop_le, Some((PropertyKeyId(6), 6)));
        variant!(dst_prop, Some((PropertyKeyId(7), 7)));
        variant!(dst_prop_ne, Some((PropertyKeyId(8), 8)));
        variant!(dst_prop_gt, Some((PropertyKeyId(9), 9)));
        variant!(dst_prop_lt, Some((PropertyKeyId(10), 10)));
        variant!(dst_prop_ge, Some((PropertyKeyId(11), 11)));
        variant!(dst_prop_le, Some((PropertyKeyId(12), 12)));
        variant!(limit, Some(13));
        variant!(skip, Some(14));
        variant!(hop2_dst_prop, Some((PropertyKeyId(15), 15)));
        variant!(hop2_dst_prop_ne, Some((PropertyKeyId(16), 16)));
        variant!(hop2_dst_prop_gt, Some((PropertyKeyId(17), 17)));
        variant!(hop2_dst_prop_lt, Some((PropertyKeyId(18), 18)));
        variant!(hop2_dst_prop_ge, Some((PropertyKeyId(19), 19)));
        variant!(hop2_dst_prop_le, Some((PropertyKeyId(20), 20)));

        for (field, changed) in variants {
            assert_plan_changed(field, &base, changed);
        }
    }

    #[test]
    fn v2_repairs_the_v1_neq_omission_without_cross_version_collision() {
        let base = plan(7);
        let mut with_neq = base.clone();
        with_neq.neq = Some(("a".to_owned(), "b".to_owned()));

        assert_eq!(
            certify_v1_legacy(&base, CommitSeq(11)).digest,
            certify_v1_legacy(&with_neq, CommitSeq(11)).digest,
            "the migration fixture must reproduce the historical omission"
        );
        assert_ne!(
            certify(&base, CommitSeq(11)).digest,
            certify(&with_neq, CommitSeq(11)).digest
        );
        assert_ne!(
            certify_v1_legacy(&base, CommitSeq(11)).digest,
            certify(&base, CommitSeq(11)).digest,
            "domain separation prevents a v1 value from being read as v2"
        );
    }

    #[test]
    fn public_execution_certificate_verifies_exact_inputs_only() {
        let bind = RelationBind::new().with_relation("KNOWS", RelationId(7));
        let certificate = GqlCertificate {
            snapshot_seq: CommitSeq(11),
            statement_digest: digest_statement("MATCH (a)-[:KNOWS]->(b) RETURN b"),
            bind_digest: digest_bind(&bind),
        };

        assert!(certificate.verifies_at("MATCH (a)-[:KNOWS]->(b) RETURN b", &bind, CommitSeq(11)));
        assert!(!certificate.verifies_at(
            "MATCH (a)-[:KNOWS]->(b) RETURN b ",
            &bind,
            CommitSeq(11)
        ));
        assert!(!certificate.verifies_at("MATCH (a)-[:KNOWS]->(b) RETURN b", &bind, CommitSeq(12)));

        let other_bind = RelationBind::new().with_relation("KNOWS", RelationId(8));
        assert!(!certificate.verifies("MATCH (a)-[:KNOWS]->(b) RETURN b", &other_bind));
    }

    #[test]
    fn result_digest_binds_plan_snapshot_count_order_and_every_row() {
        let base = plan(7);
        let certificate = certify(&base, CommitSeq(11));
        let rows = [VId(2), VId(9)];
        let digest = certificate.result_digest(&rows);

        assert!(certificate.verifies_result_digest(&rows, digest));
        assert!(!certificate.verifies_result_digest(&[VId(9), VId(2)], digest));
        assert!(!certificate.verifies_result_digest(&[VId(2), VId(8)], digest));
        assert!(!certificate.verifies_result_digest(&[VId(2)], digest));

        let other_plan = certify(&plan(8), CommitSeq(11));
        assert!(!other_plan.verifies_result_digest(&rows, digest));
        let other_snapshot = certify(&base, CommitSeq(12));
        assert!(!other_snapshot.verifies_result_digest(&rows, digest));
    }

    #[test]
    fn empty_result_digest_is_stable_and_not_a_nonempty_result() {
        let certificate = certify(&plan(7), CommitSeq(11));
        let first = certificate.result_digest(&[]);
        let second = certificate.result_digest(&[]);
        assert_eq!(first, second);
        assert!(certificate.verifies_result_digest(&[], first));
        assert!(!certificate.verifies_result_digest(&[VId(0)], first));
    }

    #[test]
    fn return_projection_tags_are_stable() {
        assert_eq!(projection_tag(ReturnProjection::Destination), 0);
        assert_eq!(projection_tag(ReturnProjection::Source), 1);
        assert_eq!(projection_tag(ReturnProjection::Hop2Destination), 2);
    }

    fn sample_result_certificate() -> NativeResultCertificate {
        NativeResultCertificate {
            plan: NativePlanCertificate {
                digest: fgdb_crypto::Digest([9; 32]),
                snapshot_seq: CommitSeq(41),
            },
            values_digest: fgdb_crypto::Digest([8; 32]),
            result_digest: fgdb_crypto::Digest([7; 32]),
            statement: "MATCH (a)-[:KNOWS]->(b) RETURN b".to_owned(),
            facade_class: NativeReadClass::TemporalPattern,
            snapshot_identity: fgdb_crypto::Digest([6; 32]),
            database_identity: fgdb_crypto::Digest([5; 32]),
        }
    }

    #[test]
    fn result_certificate_round_trip_is_exact_bytes() {
        let certificate = sample_result_certificate();
        let bytes = certificate.canonical_bytes();
        let decoded = NativeResultCertificate::decode(&bytes).unwrap();
        assert_eq!(decoded, certificate);
        assert_eq!(decoded.canonical_bytes(), bytes, "encode(decode(b)) == b");
    }

    #[test]
    fn result_certificate_decode_refuses_structural_damage_typed() {
        let bytes = sample_result_certificate().canonical_bytes();
        use super::CertificateDecodeError as E;
        // Unknown version refuses before any field is trusted. The version
        // byte immediately follows the 33-byte magic.
        let mut version = bytes.clone();
        version[super::NATIVE_RESULT_CERTIFICATE_DOMAIN_V1.len()] = 2;
        assert!(matches!(
            NativeResultCertificate::decode(&version),
            Err(E::Version(2))
        ));
        // Truncation at every prefix length refuses; no panic, no partial read.
        for cut in 0..bytes.len() {
            assert_eq!(
                NativeResultCertificate::decode(&bytes[..cut]),
                Err(E::Truncated),
                "cut={cut}"
            );
        }
        // Trailing bytes refuse: the envelope is exact.
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            NativeResultCertificate::decode(&trailing),
            Err(E::Trailing(1))
        );
        // Wrong magic and unknown facade tags refuse typed.
        let mut magic = bytes.clone();
        magic[0] ^= 0xff;
        assert_eq!(NativeResultCertificate::decode(&magic), Err(E::Magic));
        let mut facade = bytes.clone();
        let facade_at = super::NATIVE_RESULT_CERTIFICATE_DOMAIN_V1.len() + 1 + 5 * 32 + 8;
        facade[facade_at] = 9;
        assert_eq!(
            NativeResultCertificate::decode(&facade),
            Err(E::FacadeClass(9))
        );
    }

    #[test]
    fn direction_tags_are_stable() {
        assert_eq!(direction_tag(EdgeDirection::Outgoing), 0);
        assert_eq!(direction_tag(EdgeDirection::Incoming), 1);
        assert_eq!(direction_tag(EdgeDirection::Undirected), 2);
    }

    #[test]
    fn native_certificate_binds_class_plan_types_and_snapshot() {
        struct Evidence {
            class: NativeReadClass,
            bytes: Vec<u8>,
            schema: Vec<fgdb_gql::GqlParameterSpec>,
        }
        impl NativeCertificatePlan for Evidence {
            fn facade_class(&self) -> NativeReadClass {
                self.class
            }
            fn canonical_bytes(&self) -> Vec<u8> {
                self.bytes.clone()
            }
            fn parameter_schema(&self) -> &[fgdb_gql::GqlParameterSpec] {
                &self.schema
            }
        }
        let mut prepared = Evidence {
            class: NativeReadClass::Pattern,
            bytes: vec![1, 2, 3],
            schema: vec![fgdb_gql::GqlParameterSpec {
                name: "value".to_owned(),
                parameter_type: fgdb_gql::GqlParameterType::Int64,
                requires_positive: false,
                occurrences: 1,
            }],
        };
        let certificate = NativePlanCertificate::new(&prepared, CommitSeq(7));
        assert!(certificate.verifies(&prepared));
        assert!(certificate.verifies_at(&prepared, CommitSeq(7)));
        assert!(!certificate.verifies_at(&prepared, CommitSeq(8)));
        let pattern_bytes = prepared.canonical_bytes();
        prepared.class = NativeReadClass::Aggregate;
        assert_eq!(pattern_bytes, prepared.canonical_bytes());
        assert!(
            !certificate.verifies(&prepared),
            "facade domain must separate identical plan bytes"
        );
        prepared.class = NativeReadClass::Pattern;
        prepared.schema[0].parameter_type = fgdb_gql::GqlParameterType::UInt64;
        assert!(!certificate.verifies(&prepared));
        prepared.schema[0].parameter_type = fgdb_gql::GqlParameterType::Int64;
        prepared.bytes[2] = 4;
        assert!(!certificate.verifies(&prepared));
    }

    #[test]
    fn native_result_digest_binds_order_columns_and_lossless_cell_domains() {
        use crate::QueryValue;
        use fgdb_gql::{GqlParameters, GraphExactAverage};
        let plan = NativePlanCertificate {
            digest: fgdb_crypto::Digest([7; 32]),
            snapshot_seq: CommitSeq(1),
        };
        let params = GqlParameters::new();
        let columns = vec!["value".to_owned()];
        let rows = vec![vec![QueryValue::Count(1)], vec![QueryValue::Integer(2)]];
        let digest = super::native_result_digest(&plan, &params, &columns, &rows).unwrap();
        let mut reversed = rows.clone();
        reversed.reverse();
        assert_ne!(
            digest,
            super::native_result_digest(&plan, &params, &columns, &reversed).unwrap()
        );
        assert_ne!(
            digest,
            super::native_result_digest(&plan, &params, &["other".to_owned()], &rows).unwrap()
        );
        let same_magnitude = [
            QueryValue::Count(1),
            QueryValue::Integer(1),
            QueryValue::Average(GraphExactAverage::new(1, 1).unwrap()),
        ];
        let digests: Vec<_> = same_magnitude
            .into_iter()
            .map(|cell| {
                super::native_result_digest(&plan, &params, &columns, &[vec![cell]]).unwrap()
            })
            .collect();
        for i in 0..digests.len() {
            for j in 0..i {
                assert_ne!(digests[i], digests[j]);
            }
        }
    }
}
