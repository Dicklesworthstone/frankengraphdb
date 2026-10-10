//! Signed subscription reservations over the existing native maintained engine.
//!
//! A registration has one total work allowance: preparation, every circuit
//! node, replay setup, the first compressed baseline, and wire conversion.
//! Each later committed tick uses the persistently partitioned node/replay
//! allowances. Each poll separately reserves two native pulls (including a
//! possible gap restart) and one final delivery. These are per-execution
//! ceilings, not a whole-subscription-lifetime ledger.
//!
//! Native maintenance has no Warden vertex-admission hook. Conservatively,
//! graph-node work is also bounded by the signed node grant divided by the
//! largest validated source binding width. Source scans precharge physical records,
//! delta loops precharge affected records/bindings, and native fixed-hop paths
//! use the admitted maintainer's finite binding frame (a conservative maximum
//! is used only when no tighter bound is available). This constrains work
//! more than exact node accounting would. The snapshot-record ceiling is an
//! additional check inside that SAME reserved work, never another node pool.
//! Source-free folded circuits reserve no graph nodes.

use super::*;
use fgdb::{
    NativeSubscription, NativeSubscriptionSetup, PreparedNativeRead, QueryValue,
    StandingQueryError, StandingQueryFailure, SubscribeError, SubscriptionBatch, SubscriptionError,
};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlExecutionBudget, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, QueryCx};
use fgdb_warden::QueryLimits;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const REPLAY_RETENTION: [usize; 3] = [1024, 100_000, 1 << 22];

pub(crate) struct Subscription {
    pub(crate) consumer: NativeSubscription,
    pub(crate) columns: Vec<String>,
    pub(crate) generation: Generation,
    admitted: QueryLimits,
    initial: Option<Delivery>,
    registration_work: u64,
    registration_nodes: u64,
}

/// One completely checked wire batch. Frames only partition this batch; they
/// do not acquire fresh row or work allowances.
pub(crate) struct Delivery {
    pub(crate) batch: Arc<SubscriptionBatch>,
    pub(crate) entries: Vec<(i128, Vec<WireValue>)>,
}

struct Registration<'a> {
    count: &'a AtomicUsize,
    accepted: bool,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        if !self.accepted {
            self.count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn budget() -> Refusal {
    Refusal::new(ErrorCode::Budget, "subscription execution budget exhausted")
}

fn authorization(error: fgdb_warden::Error) -> Refusal {
    Refusal::new(warden_code(error), error.to_string())
}

fn is_budget(reason: StandingQueryFailure) -> bool {
    matches!(
        reason,
        StandingQueryFailure::WorkBudget
            | StandingQueryFailure::ScratchBudget
            | StandingQueryFailure::SnapshotBudget
            | StandingQueryFailure::ResultBudget
    )
}

fn native_refusal(error: StandingQueryError) -> Refusal {
    let error = match error {
        StandingQueryError::NativePrepare(source) => return query_refusal(*source),
        other => other,
    };
    let code = match &error {
        StandingQueryError::Delivery(reason)
        | StandingQueryError::Maintenance(reason)
        | StandingQueryError::Unavailable { reason, .. }
            if is_budget(*reason) =>
        {
            ErrorCode::Budget
        }
        StandingQueryError::Interrupted(_) => ErrorCode::Cancelled,
        StandingQueryError::Unsupported
        | StandingQueryError::NativeClassUnsupported { .. }
        | StandingQueryError::SetSchema(_)
        | StandingQueryError::JoinSchema(_)
        | StandingQueryError::JoinInputSchema { .. }
        | StandingQueryError::ProjectionSchema(_)
        | StandingQueryError::FilterSchema(_)
        | StandingQueryError::ReductionSchema(_)
        | StandingQueryError::WindowSchema(_)
        | StandingQueryError::GroupSchema(_) => ErrorCode::Statement,
        _ => ErrorCode::Execution,
    };
    Refusal::new(code, error.to_string())
}

impl From<SubscriptionError> for Refusal {
    fn from(error: SubscriptionError) -> Self {
        match error {
            SubscriptionError::Query(error) => native_refusal(error),
            other => Self::new(ErrorCode::Execution, other.to_string()),
        }
    }
}

fn subscribe_refusal(error: SubscribeError) -> Refusal {
    match error {
        SubscribeError::Subscription(error) => error.into(),
        other => Refusal::new(ErrorCode::Statement, other.to_string()),
    }
}

fn unrestricted(capability: &fgdb_warden::VerifiedCapability) -> Result<(), Refusal> {
    let scope = capability.predicates();
    if scope.sees_all_incidence() && scope.sees_all_fields() {
        Ok(())
    } else {
        Err(Refusal::new(
            ErrorCode::PermissionDenied,
            "a subscription requires a read capability with unrestricted label, relation and property scope",
        ))
    }
}

fn checkpoint(cx: &QueryCx) -> Result<(), Refusal> {
    cx.checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))
}

/// Work and payload reservations precede every copy. The local counter is a
/// disjoint portion already reserved from a live Warden permit.
struct Work<'a> {
    cx: &'a QueryCx,
    remaining: u64,
    scratch: u64,
    scale: u64,
}

impl Work<'_> {
    fn charge(&mut self, units: u64) -> Result<(), Refusal> {
        checkpoint(self.cx)?;
        let units = units.checked_mul(self.scale).ok_or_else(budget)?;
        self.remaining = self.remaining.checked_sub(units).ok_or_else(budget)?;
        self.scratch = self.scratch.checked_sub(units).ok_or_else(budget)?;
        Ok(())
    }

    fn bytes(&mut self, length: usize) -> Result<(), Refusal> {
        self.charge(u64::try_from(length).map_err(|_| budget())?)
    }

    fn argument(&mut self, value: &WireValue) -> Result<(), Refusal> {
        self.charge(1)?;
        match value {
            WireValue::Text(text) | WireValue::Decimal(text) => self.bytes(text.len()),
            WireValue::Bytes(bytes) => self.bytes(bytes.len()),
            WireValue::List(items) => {
                for item in items {
                    self.argument(item)?;
                }
                Ok(())
            }
            WireValue::Map(items) => {
                for (key, value) in items {
                    self.bytes(key.len())?;
                    self.argument(value)?;
                }
                Ok(())
            }
            WireValue::Path { steps, .. } => self.bytes(steps.len()),
            WireValue::Vertices(ids) | WireValue::Edges(ids) => self.bytes(ids.len()),
            WireValue::Timestamp(value) => match &value.zone {
                Some(zone) => self.bytes(zone.identifier.len()),
                None => Ok(()),
            },
            WireValue::Null
            | WireValue::Bool(_)
            | WireValue::Int(_)
            | WireValue::Float(_)
            | WireValue::Vertex(_)
            | WireValue::Edge(_)
            | WireValue::WideInt(_)
            | WireValue::Count(_)
            | WireValue::Average { .. } => Ok(()),
        }
    }

    fn graph(&mut self, value: &GraphValue) -> Result<(), Refusal> {
        self.charge(1)?;
        match value {
            GraphValue::Scalar(CanonicalScalar::Text(value)) => self.bytes(value.as_str().len()),
            GraphValue::Scalar(CanonicalScalar::Bytes(value)) => self.bytes(value.as_slice().len()),
            // STRICT_PORTABLE decimals have at most 34 coefficient digits,
            // one sign and one decimal point. Reserve before formatting.
            GraphValue::Scalar(CanonicalScalar::Decimal(_)) => self.charge(36),
            GraphValue::Scalar(CanonicalScalar::Timestamp(value)) => match value.zone() {
                Some(zone) => self.bytes(zone.identifier().len()),
                None => Ok(()),
            },
            GraphValue::Scalar(
                CanonicalScalar::Null
                | CanonicalScalar::Bool(_)
                | CanonicalScalar::Int(_)
                | CanonicalScalar::Float(_),
            )
            | GraphValue::Vertex(_)
            | GraphValue::Edge(_) => Ok(()),
            GraphValue::Path(path) => self.bytes(path.steps().len()),
            GraphValue::Vertices(ids) => self.bytes(ids.len()),
            GraphValue::Edges(ids) => self.bytes(ids.len()),
            GraphValue::List(items) => {
                for item in items.iter() {
                    self.graph(item)?;
                }
                Ok(())
            }
            GraphValue::Map { keys, values } => {
                for (key, value) in keys.iter().zip(values.iter()) {
                    self.bytes(key.len())?;
                    self.graph(value)?;
                }
                Ok(())
            }
        }
    }
}

fn wire_entries(
    batch: &SubscriptionBatch,
    work: &mut Work<'_>,
) -> Result<Vec<(i128, Vec<WireValue>)>, Refusal> {
    work.bytes(batch.rows().len())?;
    let mut entries = Vec::with_capacity(batch.rows().len());
    for (row, weight) in batch.rows().iter() {
        work.charge(1)?;
        let weight = weight.to_i128().ok_or_else(|| {
            Refusal::new(
                ErrorCode::Execution,
                "a change weight exceeds the wire range",
            )
        })?;
        work.bytes(row.len())?;
        let mut cells = Vec::with_capacity(row.len());
        for value in row {
            match value {
                QueryValue::Value(value) => work.graph(value)?,
                QueryValue::Count(_) | QueryValue::Integer(_) | QueryValue::Average(_) => {
                    work.charge(1)?;
                }
            }
            cells.push(convert::cell(value));
        }
        entries.push((weight, cells));
    }
    Ok(entries)
}

fn row_limit(policy: GqlQueryPolicy, limits: QueryLimits) -> u64 {
    policy
        .rows
        .max_result_rows()
        .unwrap_or(u64::MAX)
        .min(limits.max_rows)
}

fn check_rows(batch: &SubscriptionBatch, maximum: u64) -> Result<u64, Refusal> {
    let rows = u64::try_from(batch.rows().len()).map_err(|_| budget())?;
    if rows > maximum {
        return Err(budget());
    }
    Ok(rows)
}

/// The fixed policies installed on the producer and replay sink share one
/// admitted grant for each maintenance tick. Final support rows have only a
/// delivery ceiling; a signed row limit never truncates private circuit state.
struct Reservation {
    setup: NativeSubscriptionSetup,
    wire_work: u64,
    wire_scratch: u64,
    nodes: u64,
}

fn reserve(
    policy: GqlQueryPolicy,
    limits: QueryLimits,
    remaining_work: u64,
    remaining_scratch: u64,
    nodes: u64,
    source_width: u64,
) -> Result<Reservation, Refusal> {
    if nodes == 0 {
        return Err(budget());
    }
    let phases = nodes.checked_add(3).ok_or_else(budget)?;
    let share = remaining_work / phases;
    // Native Work and ScratchEntry each run a control callback. Their
    // combined worst case must fit this one signed work reservation.
    let native_work = share / 2;
    let scratch = (remaining_scratch / phases).min(share - native_work);
    let node_work = if source_width != 0 {
        native_work.min(limits.max_nodes / source_width / nodes)
    } else {
        native_work
    };
    if share == 0 || node_work == 0 || scratch == 0 {
        return Err(budget());
    }
    let mut maintenance = policy;
    maintenance.evaluator.max_work_units = node_work;
    maintenance.evaluator.max_scratch_entries = scratch;
    let snapshot_records = if source_width != 0 {
        policy
            .rows
            .max_snapshot_records()
            .unwrap_or(u64::MAX)
            .min(limits.max_nodes / nodes)
    } else {
        // A source-free relational group still admits owned input rows under
        // this policy. They are not graph-vertex admissions; the proven zero
        // source width already prevents any graph source from being registered.
        policy.rows.max_snapshot_records().unwrap_or(u64::MAX)
    };
    maintenance.rows = GqlExecutionBudget::new(
        snapshot_records,
        policy.rows.max_result_rows().unwrap_or(u64::MAX),
    );
    let mut delivery = policy;
    delivery.evaluator.max_work_units = native_work;
    delivery.evaluator.max_scratch_entries = scratch;
    delivery.rows = GqlExecutionBudget::new(0, row_limit(policy, limits));
    let reserved_nodes = if source_width != 0 {
        nodes
            .checked_mul(node_work)
            .and_then(|value| value.checked_mul(source_width))
            .ok_or_else(budget)?
    } else {
        0
    };
    Ok(Reservation {
        setup: NativeSubscriptionSetup {
            maintenance,
            delivery,
            retention: REPLAY_RETENTION,
        },
        wire_work: share,
        wire_scratch: scratch,
        nodes: reserved_nodes,
    })
}

pub(crate) async fn subscribe(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    statement: &Execute,
) -> Result<Subscription, Refusal> {
    let _operation = db.db.enter().map_err(Refusal::from)?;
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(authorization)?;
    let mut permit = capability
        .begin_read_at(TRUNK, now)
        .map_err(authorization)?;
    refuse_element_identity(&statement.statement)?;
    unrestricted(&capability)?;
    let limits = capability.predicates().limits();
    let registration_work = db
        .query_policy
        .evaluator
        .max_work_units
        .min(limits.max_work);
    // Reserve the complete host ceiling first. All following local counters
    // are disjoint slices of this reservation, including syntax and binding.
    permit
        .charge_work_at(now, registration_work)
        .map_err(authorization)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = contexts.query();
    let mut preparation = Work {
        cx: &query,
        remaining: registration_work,
        scratch: db.query_policy.evaluator.max_scratch_entries,
        // Four logical byte passes cover header/native preparation, the
        // structural bind/prepass, and binding the registered definition.
        scale: 4,
    };
    preparation.charge(1)?;
    preparation.bytes(statement.statement.len())?;
    for (name, value) in &statement.parameters {
        preparation.bytes(name.len())?;
        preparation.argument(value)?;
    }
    let mut guard = db.db.write(cx).await.map_err(Refusal::from)?;
    let generation = guard.generation();
    permit.checkpoint_at(unix_millis()).map_err(authorization)?;
    let parameters = convert::parameters(&statement.parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    let prepared = PreparedNativeRead::prepare_subscription(
        &query,
        &statement.statement,
        &parameters,
        db.symbols.clone(),
    )
    .map_err(subscribe_refusal)?;
    let (nodes, source_width) = prepared
        .subscription_footprint(&query, &parameters)
        .map_err(native_refusal)?;
    let reservation = reserve(
        db.query_policy,
        limits,
        preparation.remaining,
        preparation.scratch,
        nodes,
        source_width,
    )?;
    permit
        .charge_nodes_at(unix_millis(), reservation.nodes)
        .map_err(authorization)?;
    if db
        .subscriptions
        .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < db.max_subscriptions).then_some(count + 1)
        })
        .is_err()
    {
        return Err(Refusal::new(
            ErrorCode::Budget,
            "this database's subscription registrations are exhausted until reopen",
        ));
    }
    let mut registration = Registration {
        count: &db.subscriptions,
        accepted: false,
    };
    let (consumer, (columns, initial)) = prepared.subscribe_replaying(
        &mut guard,
        &query,
        &parameters,
        reservation.setup,
        |names, batch| {
            let rows = check_rows(batch, row_limit(db.query_policy, limits))?;
            permit
                .charge_rows_at(unix_millis(), rows)
                .map_err(authorization)?;
            let mut work = Work {
                cx: &query,
                remaining: reservation.wire_work,
                scratch: reservation.wire_scratch,
                scale: 1,
            };
            work.bytes(names.len())?;
            for name in names {
                work.bytes(name.len())?;
            }
            let columns = names.to_vec();
            let entries = wire_entries(batch, &mut work)?;
            permit.checkpoint_at(unix_millis()).map_err(authorization)?;
            checkpoint(&query)?;
            generation.check().map_err(Refusal::from)?;
            Ok::<_, Refusal>((
                columns,
                Delivery {
                    batch: Arc::clone(batch),
                    entries,
                },
            ))
        },
    )?;
    registration.accepted = true;
    Ok(Subscription {
        consumer,
        columns,
        generation,
        admitted: limits,
        initial: Some(initial),
        registration_work,
        registration_nodes: reservation.nodes,
    })
}

pub(crate) async fn poll(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    subscription: &mut Subscription,
) -> Result<Option<Delivery>, Refusal> {
    let _operation = subscription.generation.enter().map_err(Refusal::from)?;
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(authorization)?;
    let mut permit = capability
        .begin_read_at(TRUNK, now)
        .map_err(authorization)?;
    unrestricted(&capability)?;
    let current = capability.predicates().limits();
    let limits = QueryLimits {
        max_nodes: current.max_nodes.min(subscription.admitted.max_nodes),
        max_work: current.max_work.min(subscription.admitted.max_work),
        max_rows: current.max_rows.min(subscription.admitted.max_rows),
    };
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = contexts.query();
    if let Some(initial) = &subscription.initial {
        // Initial rendering happened inside registration's rollback boundary.
        // Attenuation cannot make a cached pending batch skip its original
        // admission reservation or its complete support-row ceiling.
        if subscription.registration_work > limits.max_work
            || subscription.registration_nodes > limits.max_nodes
        {
            return Err(budget());
        }
        permit
            .charge_work_at(now, subscription.registration_work)
            .map_err(authorization)?;
        permit
            .charge_nodes_at(now, subscription.registration_nodes)
            .map_err(authorization)?;
        let rows = check_rows(&initial.batch, row_limit(db.query_policy, limits))?;
        permit.charge_rows_at(now, rows).map_err(authorization)?;
        checkpoint(&query)?;
        subscription.generation.check().map_err(Refusal::from)?;
        permit.checkpoint_at(unix_millis()).map_err(authorization)?;
        return Ok(subscription.initial.take());
    }
    let maximum = db
        .query_policy
        .evaluator
        .max_work_units
        .min(limits.max_work);
    let share = maximum / 3;
    let native_work = share / 2;
    let scratch = (db.query_policy.evaluator.max_scratch_entries / 3).min(share - native_work);
    if native_work == 0 || scratch == 0 {
        return Err(budget());
    }
    // Two independent pulls have disjoint prepaid portions of ONE execution.
    // A gap restart can never retry with a fresh whole signed allowance.
    permit
        .charge_work_at(now, share * 3)
        .map_err(authorization)?;
    let mut policy = db.query_policy;
    policy.evaluator.max_work_units = native_work;
    policy.evaluator.max_scratch_entries = scratch;
    policy.rows = GqlExecutionBudget::new(0, row_limit(db.query_policy, limits));
    let guard = db.db.read(cx).await.map_err(Refusal::from)?;
    subscription.generation.check().map_err(Refusal::from)?;
    permit.checkpoint_at(unix_millis()).map_err(authorization)?;
    let batch = match subscription.consumer.poll(&guard, &query, policy) {
        Err(SubscriptionError::Query(
            StandingQueryError::DeltaUnavailable { .. }
            | StandingQueryError::DeltaGap { .. }
            | StandingQueryError::ReplayGap { .. },
        )) => {
            permit.checkpoint_at(unix_millis()).map_err(authorization)?;
            subscription.consumer.restart_from_current()?;
            subscription.consumer.poll(&guard, &query, policy)?
        }
        other => other?,
    };
    let Some(batch) = batch else {
        permit.checkpoint_at(unix_millis()).map_err(authorization)?;
        return Ok(None);
    };
    // Native poll intentionally reuses a pending Arc without reapplying its
    // policy. This explicit check also governs that redelivery path.
    let rows = check_rows(&batch, policy.rows.max_result_rows().unwrap_or(u64::MAX))?;
    permit
        .charge_rows_at(unix_millis(), rows)
        .map_err(authorization)?;
    let entries = wire_entries(
        &batch,
        &mut Work {
            cx: &query,
            remaining: share,
            scratch,
            scale: 1,
        },
    )?;
    permit.checkpoint_at(unix_millis()).map_err(authorization)?;
    checkpoint(&query)?;
    subscription.generation.check().map_err(Refusal::from)?;
    Ok(Some(Delivery { batch, entries }))
}

#[cfg(test)]
mod tests;
