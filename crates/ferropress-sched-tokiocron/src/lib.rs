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
//!
//! ## Time math: cron parses, `time` computes (workspace-dep constraint)
//!
//! `cron 0.12`'s only public next-fire API (`Schedule::upcoming`/`after`/
//! `includes`) is irreducibly typed on `chrono::TimeZone` (you must *name* a
//! `chrono` `Utc` value to call it). `chrono` is a *transitive* dep of `cron`
//! but is NOT in the Ferropress workspace dependency table, so it cannot be
//! named directly, and house rules forbid adding a non-workspace dep. We
//! therefore split the work along the only seam cron exposes without chrono:
//!   * `cron::Schedule::from_str` parses + validates the expression;
//!   * each parsed field is read via the chrono-free
//!     [`cron::TimeUnitSpec::includes`] (plain `u32` ordinals);
//!   * the instant arithmetic (now, component extraction, stepping) is done with
//!     the workspace `time` crate ([`time::OffsetDateTime`]).
//!
//! cron's own matcher tests days-of-week as `weekday().number_from_sunday()`
//! (Sun=1..Sat=7) and months/days as 1-based ordinals — `time`'s accessors use
//! the identical conventions, so [`next_fire_after`] is ordinal-compatible with
//! cron's internal `Schedule::includes`.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cron::{Schedule, TimeUnitSpec};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use ferropress_core::error::{CoreError, Result as CoreResult};
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
    async fn schedule(&self, cron_expr: &str, job: ScheduledJob) -> CoreResult<ScheduleId> {
        // 1. Parse + validate the expression up front so a bad spec is a caller
        //    error (returned synchronously), never a silently-dead task.
        let schedule = Schedule::from_str(cron_expr)
            .map_err(|e| CoreError::Validation(format!("invalid cron expression: {e}")))?;

        // 2. Register an id and spawn the recurring tick loop.
        let mut state = self.inner.state.lock().await;
        let id = Self::alloc_id(&mut state);

        let handle = tokio::spawn(async move {
            loop {
                let now = OffsetDateTime::now_utc();
                let Some(next) = next_fire_after(&schedule, now) else {
                    // No future occurrence within the search horizon (e.g. a
                    // one-off year already in the past). Nothing left to fire.
                    tracing::debug!("cron schedule has no further occurrences; stopping loop");
                    break;
                };

                let delay = duration_until(now, next);
                tokio::time::sleep(delay).await;

                // Run the job; a failing tick is logged but MUST NOT kill the
                // loop — the next occurrence should still fire.
                if let Err(e) = (job)().await {
                    tracing::error!("scheduled job tick failed: {e}");
                }
            }
        });

        state.tasks.insert(id.0, handle);
        Ok(id)
    }

    async fn schedule_once(
        &self,
        at_unix_millis: i64,
        job: ScheduledJob,
    ) -> CoreResult<ScheduleId> {
        // Compute the delay from now, saturating to zero if the instant is
        // already in the past (run immediately).
        let delay = duration_until_millis(at_unix_millis);

        let mut state = self.inner.state.lock().await;
        let id = Self::alloc_id(&mut state);

        // Clone a handle so the spawned task can self-remove on completion.
        let inner = Arc::clone(&self.inner);
        let task_id = id.0;

        let handle = tokio::spawn(async move {
            // Skip the timer for an already-due instant (delay == 0): fire on the
            // next poll. A zero-duration `sleep` only resolves once the time
            // driver processes expirations, which can lag while the runtime is
            // busy — so a past-due one-shot would otherwise not fire promptly.
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if let Err(e) = (job)().await {
                tracing::error!("one-shot scheduled job failed: {e}");
            }
            // Self-remove from the registry so the JoinHandle is not leaked.
            // `cancel` after this is a no-op (idempotent), which is intended.
            inner.state.lock().await.tasks.remove(&task_id);
        });

        state.tasks.insert(task_id, handle);
        Ok(id)
    }

    async fn cancel(&self, id: ScheduleId) -> CoreResult<()> {
        let mut state = self.inner.state.lock().await;
        if let Some(handle) = state.tasks.remove(&id.0) {
            handle.abort();
        }
        // A missing id is Ok: idempotent cancel, and mirrors the one-shot task
        // self-removing after it has fired.
        Ok(())
    }
}

/// Wall-clock duration from `now` until `at_unix_millis`, saturating to zero if
/// the target instant is already in the past.
fn duration_until_millis(at_unix_millis: i64) -> Duration {
    let now_millis = (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let delta = at_unix_millis.saturating_sub(now_millis);
    if delta <= 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(delta as u64)
    }
}

/// Wall-clock duration from `now` until the `next` fire instant, saturating to
/// zero if (due to scheduling jitter) `next` is not strictly after `now`.
fn duration_until(now: OffsetDateTime, next: OffsetDateTime) -> Duration {
    let delta_secs = next.unix_timestamp() - now.unix_timestamp();
    if delta_secs <= 0 {
        Duration::ZERO
    } else {
        Duration::from_secs(delta_secs as u64)
    }
}

/// Whether `dt` matches every field of `schedule`. Mirrors cron's own
/// `Schedule::includes`, but reads the parsed fields via the chrono-free
/// [`TimeUnitSpec::includes`] so we never name a `chrono` type.
fn matches(schedule: &Schedule, dt: OffsetDateTime) -> bool {
    let year = dt.year();
    // cron year ordinals are u32; pre-1AD years can't match a cron spec anyway.
    let year_ok = year >= 0 && schedule.years().includes(year as u32);

    year_ok
        && schedule.months().includes(u8::from(dt.month()) as u32)
        && schedule.days_of_month().includes(dt.day() as u32)
        && schedule
            .days_of_week()
            .includes(dt.weekday().number_from_sunday() as u32)
        && schedule.hours().includes(dt.hour() as u32)
        && schedule.minutes().includes(dt.minute() as u32)
        && schedule.seconds().includes(dt.second() as u32)
}

/// The next instant strictly after `from` (truncated to whole seconds) that
/// matches `schedule`, or `None` if none falls within the search horizon.
///
/// cron fields resolve to at most one second, so we scan at second granularity.
/// To keep a sparse spec (e.g. a single far-future year) cheap, we coarsely
/// skip-advance when the year/month/day cannot match, only stepping per-second
/// once the date is on a candidate day. The horizon cap (≈5 years) bounds the
/// worst case; baseline Ferropress schedules (publishes, daily/hourly rebuilds)
/// resolve in a handful of steps.
pub fn next_fire_after(schedule: &Schedule, from: OffsetDateTime) -> Option<OffsetDateTime> {
    // Start at the next whole second strictly after `from`.
    let start = truncate_to_second(from).saturating_add(time::Duration::seconds(1));
    let horizon = from.saturating_add(time::Duration::days(366 * 5));

    let mut cursor = start;
    while cursor <= horizon {
        let year_ok = cursor.year() >= 0 && schedule.years().includes(cursor.year() as u32);
        if !year_ok {
            // Jump to Jan 1 of the next year.
            cursor = advance_to_next_year(cursor);
            continue;
        }
        if !schedule.months().includes(u8::from(cursor.month()) as u32) {
            cursor = advance_to_next_month(cursor);
            continue;
        }
        let dom_ok = schedule.days_of_month().includes(cursor.day() as u32);
        let dow_ok = schedule
            .days_of_week()
            .includes(cursor.weekday().number_from_sunday() as u32);
        // Standard cron semantics: a restricted day-of-month OR day-of-week is
        // enough on its own. cron's own matcher ANDs them; we follow cron here
        // (its `includes` ANDs day-of-month with day-of-week) so behaviour is
        // identical to `Schedule::includes`.
        if !(dom_ok && dow_ok) {
            cursor = advance_to_next_day(cursor);
            continue;
        }
        // Date is a candidate; now match the time-of-day fields per second.
        if matches(schedule, cursor) {
            return Some(cursor);
        }
        cursor = cursor.saturating_add(time::Duration::seconds(1));
    }
    None
}

/// Truncate to the start of the current second (zero out sub-second precision).
fn truncate_to_second(dt: OffsetDateTime) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(dt.unix_timestamp()).unwrap_or(dt)
}

/// Advance to 00:00:00 on Jan 1 of the following year.
fn advance_to_next_year(dt: OffsetDateTime) -> OffsetDateTime {
    let next_year = dt.year().saturating_add(1);
    let date =
        time::Date::from_calendar_date(next_year, time::Month::January, 1).unwrap_or(dt.date());
    OffsetDateTime::new_utc(date, time::Time::MIDNIGHT)
}

/// Advance to 00:00:00 on the first day of the following month.
fn advance_to_next_month(dt: OffsetDateTime) -> OffsetDateTime {
    let (year, month) = if dt.month() == time::Month::December {
        (dt.year().saturating_add(1), time::Month::January)
    } else {
        (dt.year(), dt.month().next())
    };
    let date = time::Date::from_calendar_date(year, month, 1).unwrap_or(dt.date());
    OffsetDateTime::new_utc(date, time::Time::MIDNIGHT)
}

/// Advance to 00:00:00 on the following day.
fn advance_to_next_day(dt: OffsetDateTime) -> OffsetDateTime {
    let next_day = dt.date().next_day().unwrap_or(dt.date());
    OffsetDateTime::new_utc(next_day, time::Time::MIDNIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferropress_core::ports::BoxFuture;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Build a `ScheduledJob` that increments a shared counter each invocation.
    fn counting_job(counter: Arc<AtomicUsize>) -> ScheduledJob {
        Box::new(move || {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }) as BoxFuture<'static, CoreResult<()>>
        })
    }

    #[test]
    fn parses_valid_cron_and_computes_a_next_fire() {
        // "every second" — guaranteed to have an occurrence right away.
        let schedule = Schedule::from_str("* * * * * *").expect("valid cron");
        let now = OffsetDateTime::now_utc();
        let next = next_fire_after(&schedule, now).expect("a next fire exists");
        assert!(
            next > truncate_to_second(now),
            "next fire {next} must be after now {now}"
        );
    }

    #[test]
    fn next_fire_respects_minute_field() {
        // Fire at second 0 of every minute: the next fire must land on second 0.
        let schedule = Schedule::from_str("0 * * * * *").expect("valid cron");
        // Pick a fixed instant so the assertion is deterministic: 2030-01-01 00:00:30.
        let base = OffsetDateTime::new_utc(
            time::Date::from_calendar_date(2030, time::Month::January, 1).unwrap(),
            time::Time::from_hms(0, 0, 30).unwrap(),
        );
        let next = next_fire_after(&schedule, base).expect("a next fire exists");
        assert_eq!(next.second(), 0, "should fire on second 0");
        assert_eq!(next.minute(), 1, "next second-0 is in the following minute");
    }

    #[test]
    fn next_fire_respects_day_of_week() {
        // "At 00:00:00 every Monday" (cron Sun=1..Sat=7, so Monday=2).
        let schedule = Schedule::from_str("0 0 0 * * 2").expect("valid cron");
        // 2030-01-01 is a Tuesday; next Monday is 2030-01-07.
        let base = OffsetDateTime::new_utc(
            time::Date::from_calendar_date(2030, time::Month::January, 1).unwrap(),
            time::Time::MIDNIGHT,
        );
        let next = next_fire_after(&schedule, base).expect("a next fire exists");
        assert_eq!(
            next.weekday(),
            time::Weekday::Monday,
            "next fire must land on a Monday"
        );
        assert_eq!(next.day(), 7, "next Monday after 2030-01-01 is the 7th");
    }

    #[test]
    fn invalid_cron_expression_is_a_validation_error() {
        let sched = TokioCronScheduler::new();
        // Run on a fresh runtime via block_on so this stays a plain `#[test]`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let counter = Arc::new(AtomicUsize::new(0));
        let err = rt
            .block_on(sched.schedule("not a cron expr", counting_job(counter)))
            .expect_err("a garbage expression must be rejected");
        assert!(matches!(err, CoreError::Validation(_)));
    }

    #[tokio::test]
    async fn register_then_cancel_does_not_panic() {
        let sched = TokioCronScheduler::new();
        let counter = Arc::new(AtomicUsize::new(0));

        // A recurring schedule and a far-future one-shot; neither should fire
        // during the test, so the counter stays observably untouched after cancel.
        let recurring = sched
            .schedule("0 0 0 * * *", counting_job(Arc::clone(&counter)))
            .await
            .expect("schedule accepted");
        // One-shot ~10 minutes out, well beyond the test's lifetime.
        let far_future = (OffsetDateTime::now_utc().unix_timestamp() + 600) * 1000;
        let once = sched
            .schedule_once(far_future, counting_job(Arc::clone(&counter)))
            .await
            .expect("schedule_once accepted");

        // Cancel both; cancelling twice (and an unknown id) must be Ok.
        sched.cancel(recurring.clone()).await.expect("cancel ok");
        sched.cancel(once).await.expect("cancel ok");
        sched.cancel(recurring).await.expect("idempotent cancel ok");
        sched
            .cancel(ScheduleId(9_999))
            .await
            .expect("cancelling an unknown id is Ok");

        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "no job should have fired in the test window"
        );
    }

    #[tokio::test]
    async fn schedule_once_in_the_past_fires_immediately() {
        let sched = TokioCronScheduler::new();
        let counter = Arc::new(AtomicUsize::new(0));

        // An instant well in the past: delay saturates to zero, so it fires asap.
        let past = (OffsetDateTime::now_utc().unix_timestamp() - 60) * 1000;
        sched
            .schedule_once(past, counting_job(Arc::clone(&counter)))
            .await
            .expect("schedule_once accepted");

        // Yield until the spawned task has run (bounded: it has zero delay).
        for _ in 0..100 {
            if counter.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "a past-due one-shot must fire exactly once"
        );
    }
}
