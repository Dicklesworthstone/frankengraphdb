//! Incremental bonded recovery: completion does not wait for a response window.
//!
//! This composes borrowed ATP request futures; it is not a network runtime or a
//! new wire protocol. Healthy donors refill independently. Authentication and
//! reconstruction use Chronicle's existing bounded RaptorQ/AEAD implementation.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::Poll;

use fgdb_chronicle::identity::CryptoVerificationSink;
use fgdb_chronicle::seed::SeedObjectSpec;
use fgdb_chronicle::transfer::{
    BondedPull, DonorId, PullError, PullRequest, SymbolAdmission, VerifiedObject,
};
use fgdb_types::DatabaseSecurityNamespaceId;

use super::{PullDriveError, ReplyOutcome};
use crate::driver::{SeedAcquireError, SeedCatalog, SeedObjectSource};

/// One independently cancellable, authenticated ATP request capability.
///
/// The adapter holds its narrowed ReplCx, authenticated descriptor/namespace
/// authorization and channel identities. Each future must enforce a finite
/// transport deadline and bound allocation by the fixed symbol-record length.
/// Ordinary donor loss is `Unavailable`; an elapsed request is `TimedOut`.
/// `Err` is an attempt-wide failure, such as revoked authority or region failure.
/// The shared borrow permits concurrent requests without a global transport
/// mutex. Dropping a request must cancel its borrowed work, never detach it.
pub trait SymbolTransport {
    type Error;

    fn request(
        &self,
        request: PullRequest,
    ) -> impl Future<Output = Result<ReplyOutcome, Self::Error>>;
}

/// A seed-catalog bridge with bounded, resumable, per-symbol ATP recovery.
///
/// This implements the ordinary `SeedObjectSource`, so it composes directly
/// with `SnapshotCatchup::install` and its existing object-ownership and atomic
/// application/Raft publication gates. It grants no serving or voting authority.
///
/// The catalog and its authenticated descriptor/key/retention capabilities must
/// remain pinned for this source's lifetime. Request adapters must still enforce
/// current authority under ReplCx. Cancelling an object future preserves its
/// authenticated symbols and spent budgets in this source, but drops every
/// in-flight request. Resume the SAME object with this source. A different object
/// is rejected until recovery succeeds or is explicitly abandoned. Abandonment
/// starts a new attempt; ordinary retries must never use it as a budget reset.
pub struct StreamingSeedSource<'a, C, T> {
    namespace: DatabaseSecurityNamespaceId,
    catalog: &'a C,
    transport: &'a T,
    verification: &'a mut dyn CryptoVerificationSink,
    maximum_concurrency: usize,
    current: Option<(SeedObjectSpec, BondedPull<'a>)>,
}

impl<'a, C: SeedCatalog, T: SymbolTransport> StreamingSeedSource<'a, C, T> {
    pub fn new(
        namespace: DatabaseSecurityNamespaceId,
        catalog: &'a C,
        transport: &'a T,
        verification: &'a mut dyn CryptoVerificationSink,
        maximum_concurrency: usize,
    ) -> Self {
        Self {
            namespace,
            catalog,
            transport,
            verification,
            maximum_concurrency,
            current: None,
        }
    }

    /// Diagnostic state for an unfinished object, not a durable seed receipt.
    pub fn active_object(&self) -> Option<SeedObjectSpec> {
        self.current.as_ref().map(|(object, _)| *object)
    }

    /// Accepted equations retained across cancellation of an object future.
    pub fn retained_symbols(&self) -> usize {
        self.current
            .as_ref()
            .map_or(0, |(_, pull)| pull.symbol_count())
    }

    /// Deliberately abandon an unfinished immutable-object attempt. This drops
    /// its symbols and budget lifetime; it does not release any durable ownership
    /// promise, install a root, or authorize serving. Ordinary retries must retain
    /// this state instead, just as for the windowed BondedSeedSource.
    pub fn abandon_recovery(&mut self) {
        self.current = None;
    }

    /// Restore a donor only after authenticating its recovery and authorization
    /// for the still-pinned object. Preserve its residue, ESI and all spent work.
    pub fn donor_available(
        &mut self,
        donor: DonorId,
    ) -> Result<(), SeedAcquireError<C::Error, T::Error>> {
        let (_, pull) = self
            .current
            .as_mut()
            .ok_or(SeedAcquireError::InvalidObject)?;
        pull.donor_available(donor)
            .map_err(|error| SeedAcquireError::Pull(PullDriveError::Pull(error)))
    }
}

impl<'a, C: SeedCatalog, T: SymbolTransport> SeedObjectSource for StreamingSeedSource<'a, C, T> {
    type Error = SeedAcquireError<C::Error, T::Error>;

    #[allow(clippy::manual_async_fn)]
    fn recover(
        &mut self,
        object: SeedObjectSpec,
    ) -> impl Future<Output = Result<VerifiedObject, Self::Error>> {
        async move {
            if self.maximum_concurrency == 0 {
                return Err(SeedAcquireError::Pull(PullDriveError::InvalidWindow));
            }
            if let Some((active, _)) = &self.current {
                if *active != object {
                    return Err(SeedAcquireError::RecoveryInProgress);
                }
            } else {
                // The borrow is of the externally pinned catalog, NOT of this
                // movable source. The retained pull contains no self-reference.
                let catalog: &'a C = self.catalog;
                let material = catalog
                    .recovery(object)
                    .map_err(SeedAcquireError::Catalog)?;
                if material.target.namespace != self.namespace {
                    return Err(SeedAcquireError::WrongNamespace);
                }
                if material.encoding.object_id() != object.object_id
                    || material.target.object_id != object.object_id
                    || material.encoding.cipher_descriptor().object_kind != object.object_kind
                    || material.encoding.cipher_descriptor().compressed_len != object.compressed_len
                {
                    return Err(SeedAcquireError::InvalidObject);
                }
                let pull = BondedPull::new(
                    material.encoding,
                    material.target,
                    material.dek,
                    material.donors,
                    material.limits,
                )
                .map_err(|error| SeedAcquireError::Pull(PullDriveError::Pull(error)))?;
                self.current = Some((object, pull));
            }
            let (_, pull) = self
                .current
                .as_mut()
                .ok_or(SeedAcquireError::InvalidObject)?;
            let recovered = recover(
                pull,
                self.transport,
                self.verification,
                self.maximum_concurrency,
            )
            .await
            .map_err(SeedAcquireError::Pull)?;
            // Only success advances to the next object. Failure/cancellation
            // retains the original budget and any terminal decoder failure.
            self.current = None;
            Ok(recovered)
        }
    }
}

struct Flight<F> {
    request: PullRequest,
    // Register every reservation before constructing any transport future: a
    // panicking adapter constructor must not strand the rest of a scheduled batch.
    future: Option<Pin<Box<F>>>,
}

struct Session<'pull, 'data, F> {
    pull: &'pull mut BondedPull<'data>,
    flights: Vec<Flight<F>>,
    processing: Option<PullRequest>,
}

impl<F> Session<'_, '_, F> {
    fn retire_processing(&mut self) {
        if let Some(request) = self.processing.take() {
            // Successful admission or donor quarantine may already retire it.
            let _ = self.pull.expire(request);
        }
    }

    fn quarantine(&mut self, donor: DonorId) -> Result<(), PullError> {
        // Cancel only this donor's requests. Contributions already authenticated
        // into BondedPull remain owned and useful to the decoder.
        self.pull.donor_failed(donor)?;
        self.flights.retain(|flight| flight.request.donor != donor);
        Ok(())
    }
}

impl<F> Drop for Session<'_, '_, F> {
    fn drop(&mut self) {
        for flight in &self.flights {
            let _ = self.pull.expire(flight.request);
        }
        self.retire_processing();
        // All borrowed futures are dropped before the caller regains the pull.
    }
}

// Bound ready-only work per executor poll, even against instant timeout replies.
// No clock or detached task is introduced; the owning runtime supplies the waker.
async fn cooperate() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

/// Recover as soon as enough authenticated equations are available.
///
/// At most min(maximum_concurrency, pull.in_flight_limit()) borrowed requests
/// exist. Each donor has a separate cap, so when the global window can cover all
/// donors, a stalled donor cannot progressively take every slot. Smaller windows
/// rely on the adapter's per-request deadlines to reach unscheduled donors.
/// Storage, ingress verification, request, ESI and decode budgets remain those
/// of the caller-owned pull and survive cancellation/retry without resetting.
///
/// Outstanding useful replies are drained even when no further requests can be
/// scheduled. On completion, cancellation or failure, all owned request credits
/// are retired and all borrowed futures are dropped. Verified symbols survive.
/// This returns bytes only; callers must still publish ownership and the exact
/// snapshot/application/Raft root before acknowledging installation.
pub async fn recover<T: SymbolTransport>(
    pull: &mut BondedPull<'_>,
    transport: &T,
    verification: &mut dyn CryptoVerificationSink,
    maximum_concurrency: usize,
) -> Result<VerifiedObject, PullDriveError<T::Error>> {
    if maximum_concurrency == 0 {
        return Err(PullDriveError::InvalidWindow);
    }
    if pull.pending_count() != 0 {
        return Err(PullDriveError::OutstandingRequests);
    }
    let concurrency = maximum_concurrency.min(pull.in_flight_limit());
    let mut flights = Vec::new();
    flights
        .try_reserve_exact(concurrency)
        .map_err(|_| PullDriveError::Pull(PullError::AllocationFailed))?;
    let mut session = Session {
        pull,
        flights,
        processing: None,
    };
    let mut next_poll = 0;
    let mut completed = 0usize;
    loop {
        if let Some(object) = session
            .pull
            .try_recover(verification)
            .map_err(PullDriveError::Pull)?
        {
            return Ok(object);
        }
        let capacity = concurrency - session.flights.len();
        let requests = match session.pull.schedule_bonded(capacity, concurrency) {
            Ok(requests) => requests,
            // The last requested symbols may still finish the object. Exhausted
            // issuance does not revoke their existing authenticated reservations.
            Err(
                PullError::RequestBudget
                | PullError::SymbolSpaceExhausted
                | PullError::NoAvailableDonor,
            ) if !session.flights.is_empty() => Vec::new(),
            Err(error) => return Err(PullDriveError::Pull(error)),
        };
        let start = session.flights.len();
        session
            .flights
            .extend(requests.into_iter().map(|request| Flight {
                request,
                future: None,
            }));
        for flight in &mut session.flights[start..] {
            flight.future = Some(Box::pin(transport.request(flight.request)));
        }
        if session.flights.is_empty() {
            return Err(PullDriveError::NoCapacity);
        }
        let (index, result) = poll_fn(|cx| {
            let count = session.flights.len();
            for offset in 0..count {
                let index = (next_poll + offset) % count;
                if let Some(future) = &mut session.flights[index].future {
                    if let Poll::Ready(result) = future.as_mut().poll(cx) {
                        next_poll = (index + 1) % count;
                        return Poll::Ready((index, result));
                    }
                }
            }
            Poll::Pending
        })
        .await;
        let request = session.flights[index].request;
        session.processing = Some(request);
        // The processing guard owns its reservation across verification errors,
        // sink panics and destruction of the completed future.
        session.flights.swap_remove(index);
        match result.map_err(PullDriveError::Transport)? {
            ReplyOutcome::Record(bytes) => {
                match session.pull.accept_reply(request, &bytes, verification) {
                    Ok(SymbolAdmission::Added | SymbolAdmission::Duplicate) => {}
                    Err(
                        PullError::RecordLength
                        | PullError::UnrequestedSymbol
                        | PullError::ConflictingSymbol
                        | PullError::Symbol(_),
                    ) => {
                        session
                            .quarantine(request.donor)
                            .map_err(PullDriveError::Pull)?;
                    }
                    Err(error) => return Err(PullDriveError::Pull(error)),
                }
            }
            ReplyOutcome::Unavailable => {
                session
                    .quarantine(request.donor)
                    .map_err(PullDriveError::Pull)?;
            }
            ReplyOutcome::TimedOut => {}
        }
        session.retire_processing();
        completed += 1;
        if completed == 64 {
            cooperate().await;
            completed = 0;
        }
    }
}
