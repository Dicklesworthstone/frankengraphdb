//! Bounded, resumable I/O execution for Chronicle's authenticated bonded pulls.
//!
//! The ATP adapter executes a window concurrently under its ReplCx region and
//! returns at most one response per requested coordinate. Window completion is a
//! transport deadline, not a requirement that every donor answer. Immutable
//! authenticated symbols survive timeouts, donor failures and cancelled futures.

pub mod streaming;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use fgdb_chronicle::identity::CryptoVerificationSink;
use fgdb_chronicle::transfer::{
    BondedPull, DonorId, PullError, PullRequest, SymbolAdmission, VerifiedObject,
};

#[derive(Debug)]
pub enum ReplyOutcome {
    Record(Vec<u8>),
    /// Abandon this coordinate, but retain the donor for later repair requests.
    TimedOut,
    /// Quarantine this donor until the caller explicitly authenticates recovery
    /// and calls BondedPull::donor_available. Other donors retain their residues.
    Unavailable,
}

#[derive(Debug)]
pub struct PullReply {
    pub request: PullRequest,
    pub outcome: ReplyOutcome,
}

/// Adapter to the existing ATP transport, not a replacement wire protocol.
///
/// Route the bounded request window concurrently. Authenticate the channel's
/// donor identity and encoding/namespace authorization; do not derive identity
/// from response bytes. Decode under the negotiated fixed symbol-record length
/// and cap the number of replies before allocating. A window must have a finite,
/// runtime-supplied deadline. Missing replies count as timeouts. Dropping the
/// exchange future must cancel its borrowed region, never detach network tasks.
pub trait PullTransport {
    type Error;

    fn exchange(
        &mut self,
        requests: &[PullRequest],
    ) -> impl Future<Output = Result<Vec<PullReply>, Self::Error>>;
}

#[derive(Debug)]
pub enum PullDriveError<E> {
    Pull(PullError),
    Transport(E),
    InvalidWindow,
    /// Do not steal credits from a different driver that already owns requests.
    OutstandingRequests,
    TooManyReplies,
    ForeignReply,
    DuplicateReply,
    /// The bounded decoder/storage budget cannot admit another equation.
    NoCapacity,
}

impl<E: core::fmt::Debug> core::fmt::Display for PullDriveError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis ATP pull driver: {self:?}")
    }
}

impl<E: core::fmt::Debug> core::error::Error for PullDriveError<E> {}

/// Window ownership is separate from symbol ownership. Cancellation retires
/// only this window's exact pending coordinates, never verified contributions.
struct Window<'p, 'data> {
    pull: &'p mut BondedPull<'data>,
    requests: Vec<PullRequest>,
}

impl Drop for Window<'_, '_> {
    fn drop(&mut self) {
        for request in &self.requests {
            // Accepted, donor-cancelled or already-expired coordinates need no
            // release. expire checks the complete identity before changing credit.
            let _ = self.pull.expire(*request);
        }
    }
}

fn validate_replies<E>(
    requests: &[PullRequest],
    replies: &[PullReply],
) -> Result<(), PullDriveError<E>> {
    if replies.len() > requests.len() {
        return Err(PullDriveError::TooManyReplies);
    }
    let expected: BTreeMap<_, _> = requests.iter().map(|request| (request.esi, request)).collect();
    let mut answered = BTreeSet::new();
    // Validate the entire envelope before admitting any bytes. A malformed
    // batch cannot partially mutate the authenticated reconstruction state.
    for reply in replies {
        if expected.get(&reply.request.esi).copied() != Some(&reply.request) {
            return Err(PullDriveError::ForeignReply);
        }
        if !answered.insert(reply.request.esi) {
            return Err(PullDriveError::DuplicateReply);
        }
    }
    Ok(())
}

/// Recover one object using bounded concurrent request windows.
///
/// The pull remains owned by the caller, so verified symbols and donor stream
/// coordinates survive dropping/retrying this future. In-flight reservations
/// never survive cancellation. Request, storage, MAC-verification, ESI and decode
/// budgets are enforced by BondedPull and are not reset between retries.
///
/// Bad donor records quarantine only that donor. Resource exhaustion and final
/// object-authentication errors fail the attempt rather than being relabeled as
/// an ordinary timeout. Authentication precedes symbol admission and decoding.
pub async fn recover<T: PullTransport>(
    pull: &mut BondedPull<'_>,
    transport: &mut T,
    verification: &mut dyn CryptoVerificationSink,
    maximum_window: usize,
) -> Result<VerifiedObject, PullDriveError<T::Error>> {
    if maximum_window == 0 {
        return Err(PullDriveError::InvalidWindow);
    }
    if pull.pending_count() != 0 {
        return Err(PullDriveError::OutstandingRequests);
    }
    loop {
        if let Some(object) = pull.try_recover(verification).map_err(PullDriveError::Pull)? {
            return Ok(object);
        }
        let requests = pull.schedule(maximum_window).map_err(PullDriveError::Pull)?;
        if requests.is_empty() {
            return Err(PullDriveError::NoCapacity);
        }
        let window = Window { pull, requests };
        let replies = transport
            .exchange(&window.requests)
            .await
            .map_err(PullDriveError::Transport)?;
        validate_replies(&window.requests, &replies)?;
        let mut failed = BTreeSet::<DonorId>::new();
        for reply in replies {
            match reply.outcome {
                ReplyOutcome::Record(bytes) => {
                    match window.pull.accept_reply(reply.request, &bytes, verification) {
                        Ok(SymbolAdmission::Added | SymbolAdmission::Duplicate) => {}
                        Err(PullError::RecordLength
                            | PullError::UnrequestedSymbol
                            | PullError::ConflictingSymbol
                            | PullError::Symbol(_)) => {
                            failed.insert(reply.request.donor);
                        }
                        Err(error) => return Err(PullDriveError::Pull(error)),
                    }
                }
                ReplyOutcome::TimedOut => {}
                ReplyOutcome::Unavailable => {
                    failed.insert(reply.request.donor);
                }
            }
        }
        // Apply failure reports after admitting valid answers from this window.
        // A disconnected donor's earlier authenticated contributions stay useful,
        // regardless of response order; the surviving streams continue with FEC.
        for donor in failed {
            window.pull.donor_failed(donor).map_err(PullDriveError::Pull)?;
        }
        // Release missing/invalid/expired replies before requesting fresh repairs.
        drop(window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_crypto::Digest;
    use fgdb_types::ObjectId;

    fn request(esi: u32) -> PullRequest {
        PullRequest {
            donor: DonorId(1),
            object_id: ObjectId([7; 32]),
            encoding_id: Digest([0; 32]),
            source_block: 0,
            esi,
        }
    }

    #[test]
    fn reordered_subset_is_valid_and_missing_responses_are_timeouts() {
        let requests = [request(0), request(1), request(2)];
        let replies = [
            PullReply { request: requests[2], outcome: ReplyOutcome::TimedOut },
            PullReply { request: requests[0], outcome: ReplyOutcome::Unavailable },
        ];
        assert!(validate_replies::<()>(&requests, &replies).is_ok());
    }

    #[test]
    fn forged_coordinates_and_duplicate_answers_are_rejected() {
        let requests = [request(0), request(1)];
        let mut foreign = requests[0];
        foreign.donor = DonorId(2);
        assert!(matches!(validate_replies::<()>(&requests, &[PullReply {
            request: foreign, outcome: ReplyOutcome::TimedOut,
        }]), Err(PullDriveError::ForeignReply)));
        let replies = [
            PullReply { request: requests[0], outcome: ReplyOutcome::TimedOut },
            PullReply { request: requests[0], outcome: ReplyOutcome::Unavailable },
        ];
        assert!(matches!(validate_replies::<()>(&requests, &replies), Err(PullDriveError::DuplicateReply)));
    }
}
