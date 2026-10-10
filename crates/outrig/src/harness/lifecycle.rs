//! A session's state, published as each change happens.
//!
//! *Starting* while the container and the interpreter come up, *idle* between
//! rounds, *round running* while the model is called, *executing* while a
//! submission runs, *closing* from the close of admission until the shutdown
//! report exists, and *reported* once it does. Once closing, no state follows
//! but reported, which the shutdown publishes as the stream's last event: a
//! round still running at the close runs on, and its turns change nothing an
//! owner reads from here.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use super::ClosedBy;
use super::event::{Payload, SessionState};
use crate::events::Events;
use crate::python::host::{Admission, Gate};

/// The one place a session's state changes. Its lock is taken before the
/// interpreter's, never after.
pub(crate) struct Lifecycle {
    state: Mutex<SessionState>,
    events: Events,
    /// The interpreter's admission, once there is an interpreter.
    gate: OnceLock<Gate>,
}

impl Lifecycle {
    /// A session starting: the first event it publishes says so.
    pub(crate) fn starting(events: Events) -> Arc<Self> {
        events.emit(Payload::SessionState {
            state: SessionState::Starting,
        });
        Arc::new(Self {
            state: Mutex::new(SessionState::Starting),
            events,
            gate: OnceLock::new(),
        })
    }

    /// Where the session publishes.
    pub(crate) fn events(&self) -> &Events {
        &self.events
    }

    /// The interpreter is up: its admission is the session's.
    pub(crate) fn attach(&self, gate: Gate) {
        let _ = self.gate.set(gate);
    }

    fn state(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, state: &mut SessionState, next: SessionState) {
        if *state != next {
            *state = next;
            self.events.emit(Payload::SessionState { state: next });
        }
    }

    fn admission(&self) -> Admission {
        self.gate.get().map_or(Admission::Open, Gate::admission)
    }

    /// What closed admission, or is closing it, once something has.
    pub(crate) fn closed(&self) -> Option<ClosedBy> {
        self.gate.get().and_then(Gate::closed)
    }

    /// Move to `next`, unless the session is closing: then it stays closing,
    /// and says so if it had not yet -- the interpreter's exit closes
    /// admission without passing through here. While the exit waits for its
    /// cause nothing is published, so closing follows the exit's event.
    pub(crate) fn set(&self, next: SessionState) {
        let mut state = self.state();
        if *state == SessionState::Closing {
            return;
        }
        match self.admission() {
            Admission::Open => self.publish(&mut state, next),
            Admission::Closed(_) => self.publish(&mut state, SessionState::Closing),
            Admission::Exiting(_) => {}
        }
    }

    /// A round opens, if admission is open: round running until the guard is
    /// dropped, then idle.
    pub(crate) fn open_round(self: &Arc<Self>) -> Result<RoundGuard, ClosedBy> {
        let mut state = self.state();
        match self.admission() {
            Admission::Open => {
                self.publish(&mut state, SessionState::RoundRunning);
                Ok(RoundGuard(Arc::clone(self)))
            }
            Admission::Closed(by) => {
                self.publish(&mut state, SessionState::Closing);
                Err(by)
            }
            Admission::Exiting(by) => Err(by),
        }
    }

    /// Close admission, `by` what closed it, and publish closing. Returns at
    /// once. When this was not the close, whatever was publishes closing:
    /// an exit already seen does as its hook runs, after its event.
    pub(crate) fn close(&self, by: ClosedBy) {
        let mut state = self.state();
        if self.gate.get().is_some_and(|gate| gate.close(by)) {
            self.publish(&mut state, SessionState::Closing);
        }
    }

    /// The interpreter has exited, which closed admission: publish closing.
    pub(crate) fn exited(&self) {
        self.set(SessionState::Closing);
    }
}

/// A round in progress. Dropped -- the round returned, or its future was --
/// the session is idle again, unless it is closing.
pub(crate) struct RoundGuard(Arc<Lifecycle>);

impl Drop for RoundGuard {
    fn drop(&mut self) {
        self.0.set(SessionState::Idle);
    }
}
