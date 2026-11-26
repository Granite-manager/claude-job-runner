//! The polling loop: wait for a free slot, claim an agentic user message,
//! run it in a fresh workspace, record the answer, delete the workspace,
//! repeat.
//!
//! Up to `max_concurrent_jobs` jobs run at the same time. A row is only
//! claimed (marked `pending`) once a slot is free for it, so the table
//! reflects what is actually running rather than what has been queued
//! inside the process.

use std::error::Error;
use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::claude::{ClaudeRunner, RunRequest};
use crate::job::{Job, JobOutcome, MessageId, prompt_with_history};
use crate::pages::{self, Page};
use crate::progress::Progress;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::time::Instant;
use crate::store::JobStore;
use crate::workspace::{Workspace, WorkspaceRoot};

/// Drives jobs from the store through a [`ClaudeRunner`].
///
/// Cloning is cheap: clones share the store, runner, and concurrency slots.
#[derive(Debug)]
pub struct Worker<R> {
    inner: Arc<Inner<R>>,
}

#[derive(Debug)]
struct Inner<R> {
    store: JobStore,
    runner: R,
    root: WorkspaceRoot,
    poll_interval: Duration,
    /// One permit per job that may run at the same time.
    slots: Arc<Semaphore>,
    max_concurrent_jobs: NonZeroUsize,
}

impl<R> Clone for Worker<R> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<R: ClaudeRunner + 'static> Worker<R> {
    /// Assembles a worker from its parts.
    #[must_use]
    pub fn new(
        store: JobStore,
        runner: R,
        root: WorkspaceRoot,
        poll_interval: Duration,
        max_concurrent_jobs: NonZeroUsize,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                runner,
                root,
                poll_interval,
                slots: Arc::new(Semaphore::new(max_concurrent_jobs.get())),
                max_concurrent_jobs,
            }),
        }
    }

    /// Polls until `shutdown` is cancelled, running up to
    /// `max_concurrent_jobs` jobs at once.
    ///
    /// Jobs in flight when shutdown arrives are aborted and recorded as
    /// failed, so the table never keeps a stale `pending` row. This returns
    /// only after every such job has been recorded.
    pub async fn run(&self, shutdown: CancellationToken) {
        info!(
            poll_interval = ?self.inner.poll_interval,
            max_concurrent_jobs = self.inner.max_concurrent_jobs,
            "worker started"
        );
        let mut in_flight = JoinSet::new();
        loop {
            reap_finished(&mut in_flight);

            // Hold a slot *before* touching the table, so a row is only
            // marked pending once the job can actually start.
            let permit = tokio::select! {
                () = shutdown.cancelled() => break,
                permit = Arc::clone(&self.inner.slots).acquire_owned() => {
                    // The semaphore is never closed, so this cannot fail.
                    let Ok(permit) = permit else { break };
                    permit
                }
            };

            match self.inner.store.claim_next().await {
                Ok(Some(job)) => {
                    let worker = self.clone();
                    let shutdown = shutdown.clone();
                    in_flight.spawn(async move {
                        worker.process(job, &shutdown).await;
                        drop(permit);
                    });
                    // Drain the queue before sleeping again.
                    continue;
                }
                Ok(None) => {}
                Err(err) => error!(error = %chain(&err), "could not poll for jobs"),
            }
            drop(permit);
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = sleep(self.inner.poll_interval) => {}
            }
        }

        // Running jobs see the cancelled token and record themselves failed.
        while let Some(finished) = in_flight.join_next().await {
            log_join_result(finished);
        }
        info!("worker stopped");
    }

    /// Executes one already-claimed job and stores its outcome.
    pub async fn process(&self, job: Job, shutdown: &CancellationToken) -> JobOutcome {
        info!(job = %job.id, "starting job");
        let _active = crate::activity::job_started();
        let (outcome, pages) = self.execute(&job, shutdown).await;
        match &outcome {
            JobOutcome::Success { .. } => info!(job = %job.id, "job succeeded"),
            JobOutcome::Failed { error, .. } => warn!(job = %job.id, error, "job failed"),
        }
        if let Err(err) = self
            .inner
            .store
            .finish_with_pages(job.id, &outcome, &pages)
            .await
        {
            error!(job = %job.id, error = %chain(&err), "could not record job outcome");
        }
        outcome
    }

    async fn execute(&self, job: &Job, shutdown: &CancellationToken) -> (JobOutcome, Vec<Page>) {
        let workspace = match Workspace::create(&self.inner.root, job.id).await {
            Ok(workspace) => workspace,
            Err(err) => return (JobOutcome::failed(chain(&err)), Vec::new()),
        };

        // A follow-up carries the conversation so far.
        let history = match self.inner.store.history(job.id, HISTORY_TURNS).await {
            Ok(history) => history,
            Err(err) => {
                warn!(job = %job.id, error = %chain(&err), "could not read the conversation");
                Vec::new()
            }
        };
        let prompt = prompt_with_history(&history, &job.content);

        // Steps go to the question's payload as they happen, at most every
        // PROGRESS_EVERY, so the person waiting sees what the agent does.
        let (steps_tx, steps_rx) = mpsc::unbounded_channel();
        let recorder = tokio::spawn(record_progress(
            self.inner.store.clone(),
            job.id,
            steps_rx,
        ));

        let request = RunRequest {
            question: job.id,
            prompt: &prompt,
            workspace: workspace.path(),
            requester: job.requester,
            steps: Some(&steps_tx),
        };
        let outcome = tokio::select! {
            run = self.inner.runner.run(request) => match run {
                Ok(report) => {
                    info!(
                        job = %job.id,
                        cost_usd = ?report.cost_usd,
                        turns = ?report.num_turns,
                        denials = report.permission_denials,
                        session = ?report.session_id,
                        "claude finished"
                    );
                    report.into_outcome()
                }
                Err(err) => JobOutcome::failed(chain(&err)),
            },
            () = shutdown.cancelled() => JobOutcome::failed("runner shut down before the job finished"),
        };
        drop(steps_tx);
        if let Err(err) = recorder.await {
            warn!(job = %job.id, error = %err, "progress recorder ended abnormally");
        }

        let pages = if matches!(outcome, JobOutcome::Success { .. }) {
            pages::collect(workspace.path()).await
        } else {
            Vec::new()
        };
        if !pages.is_empty() {
            info!(job = %job.id, pages = pages.len(), "job made pages");
        }

        if let Err(err) = workspace.remove().await {
            warn!(job = %job.id, error = %chain(&err), "could not remove workspace");
        }
        (outcome, pages)
    }
}

/// Earlier questions of the conversation a follow-up's prompt carries.
const HISTORY_TURNS: u32 = 6;

/// Least time between two progress writes of one job.
const PROGRESS_EVERY: Duration = Duration::from_millis(1500);

/// Writes a job's steps to its row as they arrive, batched, and once more
/// when the run ends so the last steps are kept.
async fn record_progress(store: JobStore, id: MessageId, mut steps: UnboundedReceiver<String>) {
    let mut progress = Progress::default();
    let mut dirty = false;
    let mut last_write: Option<Instant> = None;
    loop {
        let wait = last_write.map_or(Duration::ZERO, |at| PROGRESS_EVERY.saturating_sub(at.elapsed()));
        let next = if dirty {
            match tokio::time::timeout(wait, steps.recv()).await {
                Ok(next) => next.map(Some),
                Err(_elapsed) => Some(None),
            }
        } else {
            steps.recv().await.map(Some)
        };
        match next {
            Some(Some(step)) => {
                crate::activity::touch();
                dirty |= progress.push(crate::activity::now(), step);
                if dirty && last_write.is_none_or(|at| at.elapsed() >= PROGRESS_EVERY) {
                    write_progress(&store, id, &progress).await;
                    last_write = Some(Instant::now());
                    dirty = false;
                }
            }
            Some(None) => {
                write_progress(&store, id, &progress).await;
                last_write = Some(Instant::now());
                dirty = false;
            }
            None => break,
        }
    }
    if dirty {
        write_progress(&store, id, &progress).await;
    }
}

async fn write_progress(store: &JobStore, id: MessageId, progress: &Progress) {
    if let Err(err) = store.set_progress(id, &progress.to_json()).await {
        warn!(job = %id, error = %chain(&err), "could not record progress");
    }
}

/// Drops the results of jobs that have already finished so the set does not
/// grow without bound.
fn reap_finished(in_flight: &mut JoinSet<()>) {
    while let Some(finished) = in_flight.try_join_next() {
        log_join_result(finished);
    }
}

/// A job task only ends abnormally if it panicked; the row is then left
/// `pending` and picked up by `REQUEUE_PENDING_ON_START` next time.
fn log_join_result(result: Result<(), tokio::task::JoinError>) {
    if let Err(err) = result {
        error!(error = %err, "job task ended abnormally");
    }
}

/// Renders an error together with its source chain (`a: b: c`).
pub(crate) fn chain(err: &dyn Error) -> String {
    let mut text = err.to_string();
    let mut current = err.source();
    while let Some(source) = current {
        let _ = write!(text, ": {source}");
        current = source.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_joins_sources() {
        let inner = std::io::Error::other("disk on fire");
        let outer = crate::workspace::WorkspaceError::Io {
            action: "creating workspace",
            path: "/x".into(),
            source: inner,
        };
        assert_eq!(chain(&outer), "creating workspace /x: disk on fire");
    }
}
