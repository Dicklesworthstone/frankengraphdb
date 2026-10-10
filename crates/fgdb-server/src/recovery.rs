//! The served database's authoritative recovery owner (fgdb-t79am).
//!
//! One host-region child owns recovery, independently of client connections.
//! A write guard fences admission on Drop, including cancellation after D1/D2.
//! Recovery waits for admitted source operations, consumes the old writer, and
//! reopens through Chronicle. It never replays a client statement. A failed or
//! interrupted recovery leaves the slot fenced; there is no retry loop.

use crate::commits::CommitSignal;
use asupersync::Cx;
use asupersync::runtime::TaskHandle;
use asupersync::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use core::future::poll_fn;
use core::ops::{Deref, DerefMut};
use core::task::Poll;
use fgdb::{Database, DatabaseState};
use fgdb_protocol::body::ErrorCode;
use fgdb_types::PurposeContexts;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock as GateLock};

/// Host diagnostics, never an unauthenticated network endpoint. Recovery
/// errors are retained for the operator; public requests get a uniform class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DatabaseStatus {
    Ready { generation: u64 },
    Recovering { generation: u64 },
    Fenced { generation: u64, reason: String },
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Ready,
    Recovering,
    Fenced,
}

struct State {
    phase: Phase,
    generation: u64,
    active: usize,
    stopping: bool,
    failure: Option<String>,
}

struct Gate {
    state: GateLock<State>,
    signal: Arc<CommitSignal>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unavailable {
    Recovering,
    Fenced,
    Stopping,
    Stale,
}

impl Unavailable {
    pub(crate) fn code(self) -> ErrorCode {
        match self {
            Self::Recovering | Self::Stale => ErrorCode::DatabaseRecovering,
            Self::Fenced => ErrorCode::DatabaseUnavailable,
            Self::Stopping => ErrorCode::Draining,
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Recovering => "database is recovering; retry with a new request",
            Self::Stale => "database generation changed; start a new statement or transaction",
            Self::Fenced => "database recovery failed; the database remains unavailable",
            Self::Stopping => "database is draining",
        }
    }
}

impl core::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.message())
    }
}

fn current(state: &State, generation: u64) -> Result<(), Unavailable> {
    match state.phase {
        Phase::Recovering => Err(Unavailable::Recovering),
        Phase::Fenced => Err(Unavailable::Fenced),
        Phase::Ready if state.generation != generation => Err(Unavailable::Stale),
        Phase::Ready => Ok(()),
    }
}

/// An ephemeral generation fence, not a credential or a database reference.
/// Retained sessions and queued output keep their original value after reopen.
#[derive(Clone)]
pub(crate) struct Generation {
    gate: Arc<Gate>,
    value: u64,
}

impl Generation {
    pub(crate) fn watcher(&self) -> crate::commits::CommitWatcher {
        self.gate.signal.watcher()
    }

    pub(crate) fn check(&self) -> Result<(), Unavailable> {
        self.with_current(|| ())
    }

    /// Hold the lifecycle gate for one nonblocking transport poll. Recovery
    /// cannot invalidate the generation between its check and that poll's
    /// physical writes. The guard never survives Pending or an await.
    pub(crate) fn with_current<T>(&self, action: impl FnOnce() -> T) -> Result<T, Unavailable> {
        let state = self
            .gate
            .state
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        current(&state, self.value)?;
        Ok(action())
    }

    pub(crate) fn enter(&self) -> Result<Operation, Unavailable> {
        let mut state = self
            .gate
            .state
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        current(&state, self.value)?;
        if state.stopping {
            return Err(Unavailable::Stopping);
        }
        state.active = state.active.checked_add(1).ok_or(Unavailable::Fenced)?;
        Ok(Operation {
            generation: self.clone(),
        })
    }
}

/// One admitted source operation. This is separate from a pinned session:
/// an idle Bolt transaction cannot prevent recovery indefinitely.
pub(crate) struct Operation {
    generation: Generation,
}

impl Drop for Operation {
    fn drop(&mut self) {
        let empty = {
            let mut state = self
                .generation
                .gate
                .state
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            state.active -= 1;
            state.active == 0 && (state.phase != Phase::Ready || state.stopping)
        };
        if empty {
            self.generation.gate.signal.committed();
        }
    }
}

struct Shared {
    database: RwLock<Option<Database>>,
    gate: Arc<Gate>,
    subscriptions: Arc<AtomicUsize>,
}

impl Shared {
    fn fence(&self) {
        let changed = {
            let mut state = self
                .gate
                .state
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if state.phase != Phase::Ready {
                false
            } else if let Some(next) = state.generation.checked_add(1) {
                state.generation = next;
                state.phase = Phase::Recovering;
                true
            } else {
                state.phase = Phase::Fenced;
                state.failure = Some("database recovery generation exhausted".to_owned());
                true
            }
        };
        if changed {
            self.gate.signal.committed();
        }
    }

    fn failed(&self, reason: String) {
        {
            let mut state = self
                .gate
                .state
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            state.phase = Phase::Fenced;
            state.failure = Some(reason);
        }
        self.gate.signal.committed();
    }
}

/// One task per opened database, owned independently of the listener count.
/// The task captures only Shared, never this owner or Server, so no Arc cycle
/// can retain an unopened listener's database. Drop stops/aborts its child;
/// ordinary shutdown joins it explicitly through stop_and_join.
/// The opening Cx must be the long-lived host region, not a request region.
pub(crate) struct DatabaseSlot {
    shared: Arc<Shared>,
    worker: Mutex<Option<TaskHandle<()>>>,
}

impl DatabaseSlot {
    pub(crate) fn new(
        cx: &Cx,
        database: Database,
        signal: Arc<CommitSignal>,
        subscriptions: Arc<AtomicUsize>,
    ) -> Result<Self, crate::ServerError> {
        let shared = Arc::new(Shared {
            database: RwLock::new(Some(database)),
            gate: Arc::new(Gate {
                state: GateLock::new(State {
                    phase: Phase::Ready,
                    generation: 1,
                    active: 0,
                    stopping: false,
                    failure: None,
                }),
                signal,
            }),
            subscriptions,
        });
        let task_shared = Arc::clone(&shared);
        // Construct this before spawn, not on the child's first poll: a
        // cancelled/unadmitted task must fence its slot even if never polled.
        let task_exit = WorkerExit {
            shared: Arc::clone(&shared),
            finished: false,
        };
        let worker = cx
            .spawn(move |child| async move {
                recover(&child, task_shared, task_exit).await;
            })
            .map_err(|_| crate::ServerError::Spawn)?;
        Ok(Self {
            shared,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub(crate) fn generation(&self) -> Result<Generation, Unavailable> {
        let state = self
            .shared
            .gate
            .state
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        current(&state, state.generation)?;
        if state.stopping {
            return Err(Unavailable::Stopping);
        }
        Ok(Generation {
            gate: Arc::clone(&self.shared.gate),
            value: state.generation,
        })
    }

    pub(crate) fn enter(&self) -> Result<Operation, Unavailable> {
        self.generation()?.enter()
    }

    pub(crate) async fn read(&self, cx: &Cx) -> Result<ReadGuard<'_>, Unavailable> {
        let generation = self.generation()?;
        let guard = self
            .shared
            .database
            .read(cx)
            .await
            .map_err(|_| Unavailable::Stopping)?;
        generation.check()?;
        if guard.is_none() {
            return Err(Unavailable::Fenced);
        }
        Ok(ReadGuard { guard, generation })
    }

    pub(crate) async fn write(&self, cx: &Cx) -> Result<WriteGuard<'_>, Unavailable> {
        let generation = self.generation()?;
        let guard = self
            .shared
            .database
            .write(cx)
            .await
            .map_err(|_| Unavailable::Stopping)?;
        generation.check()?;
        if guard.is_none() {
            return Err(Unavailable::Fenced);
        }
        Ok(WriteGuard {
            guard,
            shared: &self.shared,
            generation,
        })
    }

    pub(crate) fn status(&self) -> DatabaseStatus {
        let state = self
            .shared
            .gate
            .state
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        match state.phase {
            Phase::Ready if state.stopping => DatabaseStatus::Stopped,
            Phase::Ready => DatabaseStatus::Ready {
                generation: state.generation,
            },
            Phase::Recovering => DatabaseStatus::Recovering {
                generation: state.generation,
            },
            Phase::Fenced => DatabaseStatus::Fenced {
                generation: state.generation,
                reason: state
                    .failure
                    .clone()
                    .unwrap_or_else(|| "recovery worker unavailable".to_owned()),
            },
        }
    }

    fn stop(&self) {
        self.shared
            .gate
            .state
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .stopping = true;
        self.shared.gate.signal.committed();
    }

    pub(crate) async fn stop_and_join(&self, cx: &Cx) {
        self.stop();
        if let Ok(mut worker) = self.worker.lock(cx).await
            && let Some(mut handle) = worker.take()
        {
            let _ = handle.join(cx).await;
        }
    }
}

impl Drop for DatabaseSlot {
    fn drop(&mut self) {
        self.stop();
        if let Ok(worker) = self.worker.try_lock()
            && let Some(handle) = worker.as_ref()
        {
            handle.abort();
        }
    }
}

pub(crate) struct ReadGuard<'a> {
    guard: RwLockReadGuard<'a, Option<Database>>,
    generation: Generation,
}
impl ReadGuard<'_> {
    pub(crate) fn generation(&self) -> Generation {
        self.generation.clone()
    }
}
impl Deref for ReadGuard<'_> {
    type Target = Database;
    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().expect("admitted database slot")
    }
}

pub(crate) struct WriteGuard<'a> {
    guard: RwLockWriteGuard<'a, Option<Database>>,
    shared: &'a Shared,
    generation: Generation,
}
impl WriteGuard<'_> {
    pub(crate) fn generation(&self) -> Generation {
        self.generation.clone()
    }
}
impl Deref for WriteGuard<'_> {
    type Target = Database;
    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().expect("admitted database slot")
    }
}
impl DerefMut for WriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().expect("admitted database slot")
    }
}
impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        if self
            .guard
            .as_ref()
            .is_some_and(|db| !matches!(db.state(), DatabaseState::Healthy { .. }))
        {
            // Publish the fence before releasing the write lock. A waiting
            // request cannot acquire this old handle through the gap.
            self.shared.fence();
        }
    }
}

struct WorkerExit {
    shared: Arc<Shared>,
    finished: bool,
}
impl Drop for WorkerExit {
    fn drop(&mut self) {
        if !self.finished {
            self.shared
                .failed("authoritative recovery worker was interrupted".to_owned());
        }
    }
}

async fn recover(cx: &Cx, shared: Arc<Shared>, mut exit: WorkerExit) {
    let mut watcher = shared.gate.signal.watcher();
    loop {
        let needed = poll_fn(|task| {
            loop {
                if cx.checkpoint().is_err() {
                    return Poll::Ready(false);
                }
                {
                    let state = shared
                        .gate
                        .state
                        .read()
                        .unwrap_or_else(PoisonError::into_inner);
                    if state.phase == Phase::Recovering && state.active == 0 {
                        return Poll::Ready(true);
                    }
                    if state.active == 0 && (state.stopping || state.phase == Phase::Fenced) {
                        return Poll::Ready(false);
                    }
                }
                if !watcher.poll_changed(task) {
                    return Poll::Pending;
                }
            }
        })
        .await;
        if !needed {
            exit.finished = cx.checkpoint().is_ok();
            return;
        }
        let candidate = match shared.database.write(cx).await {
            Ok(mut guard) => guard.take(),
            Err(_) => return,
        };
        let Some(candidate) = candidate else {
            shared.failed("authoritative recovery has no database handle".to_owned());
            exit.finished = true;
            return;
        };
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let recovered = match candidate.recover_authoritatively(&contexts.commit()).await {
            Ok(database) => database,
            Err(error) => {
                shared.failed(error.to_string());
                exit.finished = true;
                return;
            }
        };
        let Ok(mut guard) = shared.database.write(cx).await else {
            return;
        };
        *guard = Some(recovered);
        // Registrations belong to the consumed database. Old consumers never
        // decrement this count, so their later closure cannot spend new slots.
        shared.subscriptions.store(0, Ordering::Release);
        {
            let mut state = shared
                .gate
                .state
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            state.phase = Phase::Ready;
        }
        drop(guard);
        shared.gate.signal.committed();
    }
}

#[cfg(test)]
pub(crate) mod tests;
