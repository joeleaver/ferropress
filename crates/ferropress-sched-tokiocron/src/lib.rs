//! # ferropress-sched-tokiocron
//!
//! The baseline [`Scheduler`] adapter: an in-process scheduler built on Tokio
//! tasks plus the `cron` crate's schedule parser. Drives the two time-based
//! Ferropress needs:
//!   * **scheduled publish** — a `Status::Scheduled` post fires once at its
//!     instant ([`Scheduler::schedule_once`]), flipping to `Published` and
//!     triggering a regen;
//!   * **periodic rebuild / maintenance** — a cron expression
//!     ([`Scheduler::schedule`]).
//!
//! INVARIANT (PORT shape): the port is engine-shaped. There is NO trigger header,
//! no host webhook, no external cron service — ticks come from Tokio timers in
//! THIS process. The job itself is an opaque [`ScheduledJob`] future factory; no
//! scheduling metadata leaks into it.
//!
//! Each registered schedule owns a Tokio task; [`Scheduler::cancel`] aborts it.

// The registry fields + id allocator are the real intended structure; they are
// only read once the `todo!()` method bodies are filled in. Allow until then so
// the stub compiles clean under CI's `-D warnings`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use ferropress_core::error::Result as CoreResult;
use ferropress_core::ports::{ScheduleId, ScheduledJob, Scheduler};

/// An in-process cron/one-shot scheduler. Cloneable: clones share the same task
/// registry, so a schedule registered through one handle is cancellable through
/// another.
#[derive(Clone, Default)]
pub struct TokioCronScheduler {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Monotonic id source + the live task handles, behind one async mutex.
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    tasks: HashMap<u64, JoinHandle<()>>,
}

impl TokioCronScheduler {
    /// Build an empty scheduler. No background tasks exist until something is
    /// scheduled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next schedule id. Internal helper (locked by the caller's
    /// `await` on `state`).
    fn alloc_id(state: &mut State) -> ScheduleId {
        let id = state.next_id;
        state.next_id += 1;
        ScheduleId(id)
    }
}

#[async_trait]
impl Scheduler for TokioCronScheduler {
    async fn schedule(&self, _cron_expr: &str, _job: ScheduledJob) -> CoreResult<ScheduleId> {
        // TODO:
        //   1. cron::Schedule::from_str(cron_expr); map a parse error to
        //      CoreError::Validation.
        //   2. lock state, alloc_id, tokio::spawn a loop that computes the next
        //      `upcoming(Utc)` instant, sleeps until it (tokio::time::sleep_until),
        //      then runs (job)().await, logging any Err without killing the loop.
        //   3. insert the JoinHandle under the new id; return the id.
        let mut _state = self.inner.state.lock().await;
        todo!("parse cron expr, spawn the recurring tick task, register + return id")
    }

    async fn schedule_once(
        &self,
        _at_unix_millis: i64,
        _job: ScheduledJob,
    ) -> CoreResult<ScheduleId> {
        // TODO:
        //   1. compute the Duration from `now` to at_unix_millis (saturating to
        //      zero if already past -> run immediately).
        //   2. lock state, alloc_id, tokio::spawn: sleep that duration, then run
        //      (job)().await once, then self-remove from the registry.
        //   3. return the id.
        let mut _state = self.inner.state.lock().await;
        todo!("spawn a one-shot delay task at the given unix-millis instant")
    }

    async fn cancel(&self, _id: ScheduleId) -> CoreResult<()> {
        // TODO: lock state, remove the JoinHandle for id and .abort() it. A
        // missing id is Ok (idempotent cancel, mirrors the one-shot self-removal).
        let mut _state = self.inner.state.lock().await;
        todo!("abort + drop the task for this ScheduleId (missing id is Ok)")
    }
}
