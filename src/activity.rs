//! How busy the runner is, for the idle timer that stops its machine.
//!
//! The runner's EC2 instance is stopped while nobody asks the agent
//! anything; the Granite API starts it again when someone does. A root
//! systemd timer (`deploy/idle-stop.sh`) asks `GET /idle` every minute and
//! powers the machine off once no job has run for a while. A job counts as
//! activity from the moment it is claimed until it is recorded.

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

static RUNNING: AtomicUsize = AtomicUsize::new(0);
static LAST_ACTIVE: AtomicI64 = AtomicI64::new(0);

/// Unix seconds now.
#[must_use]
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Marks the runner active as of now (the process started, or a job did).
pub fn touch() {
    LAST_ACTIVE.store(now(), Ordering::Relaxed);
}

/// Counts a job as running until the guard is dropped.
#[must_use]
pub fn job_started() -> JobGuard {
    RUNNING.fetch_add(1, Ordering::Relaxed);
    touch();
    JobGuard(())
}

/// See [`job_started`].
#[derive(Debug)]
pub struct JobGuard(());

impl Drop for JobGuard {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::Relaxed);
        touch();
    }
}

/// What `GET /idle` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Idle {
    /// Jobs running now.
    pub running: usize,
    /// Seconds since a job last ran (or the runner started); 0 while one runs.
    pub idle_secs: i64,
}

/// How idle the runner is now.
#[must_use]
pub fn idle() -> Idle {
    let running = RUNNING.load(Ordering::Relaxed);
    let last = LAST_ACTIVE.load(Ordering::Relaxed);
    Idle {
        running,
        idle_secs: if running > 0 || last == 0 {
            0
        } else {
            (now() - last).max(0)
        },
    }
}
