use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use serde::Serialize;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{error, info};
use uuid::Uuid;

use crate::db::{BenchJob, Db};
use crate::github::CachedOctocrab;
use crate::job::{bench_main, handle_issue_comment, handle_pr_review, handle_pr_update};
use crate::runner::BenchRunner;
use crate::AppConfig;

/// A queue that keeps track of GitHub events and handles them sequentially in the background
#[derive(Clone)]
pub struct EventQueue {
    /// The job that is currently running (if any)
    active_job_id: Arc<Mutex<Option<Uuid>>>,
    /// A sender indicating that a new event has been enqueued
    event_enqueued_tx: UnboundedSender<()>,
    /// Database handle, used to persist events and recover in case of crashes
    db: Db,
}

impl EventQueue {
    /// Creates a new queue.
    ///
    /// Spawns a tokio background task to process queued events. The background task is supervised
    /// and will be restarted if it crashes (see [Self::start_and_supervise_queue_processing] for
    /// details).
    pub(crate) fn new(
        config: Arc<AppConfig>,
        db: Db,
        bench_runner: Arc<dyn BenchRunner>,
        octocrab: CachedOctocrab,
    ) -> Self {
        let (worker_tx, event_enqueued_rx) = tokio::sync::mpsc::unbounded_channel();

        let active_job_id = Arc::new(Mutex::new(None));
        let queue = Self {
            active_job_id,
            event_enqueued_tx: worker_tx,
            db,
        };

        queue.start_and_supervise_queue_processing(
            event_enqueued_rx,
            config,
            bench_runner,
            octocrab,
        );
        queue
    }

    /// Starts and supervises the background queue processing task
    fn start_and_supervise_queue_processing(
        &self,
        event_enqueued_rx: UnboundedReceiver<()>,
        config: Arc<AppConfig>,
        bench_runner: Arc<dyn BenchRunner>,
        octocrab: CachedOctocrab,
    ) {
        let queue = self.clone();
        let event_enqueued_rx = Arc::new(tokio::sync::Mutex::new(event_enqueued_rx));
        tokio::spawn(async move {
            loop {
                let background_task = queue.process_queued_events_in_background(
                    event_enqueued_rx.clone(),
                    config.clone(),
                    bench_runner.clone(),
                    octocrab.clone(),
                );

                info!("background task started for event queue");
                match background_task.await {
                    Ok(Ok(_)) => {
                        // The task finished normally, no need to restart it
                        break;
                    }
                    Ok(Err(e)) => {
                        // The task finished with an error
                        error!(
                            cause = e.to_string(),
                            "job queue background task errored, restarting in 1s"
                        );
                    }
                    Err(e) => {
                        // The task panicked or was cancelled
                        error!(
                            cause = e.to_string(),
                            "job queue background task crashed, restarting in 1s"
                        );
                    }
                }

                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    /// Spawns a tokio task to process queued events in the background.
    ///
    /// The task works based on the "let it crash" principle for errors that are caused by
    /// transient events, like the database being unavailable. Therefore, it should be supervised
    /// and restarted upon need.
    ///
    /// An important consequence of letting the task crash and restart is that we will automatically
    /// try handling the event again. Therefore we need to make sure we only crash on transient
    /// errors (otherwise the we will get caught in an infinite crash and restart loop).
    fn process_queued_events_in_background(
        &self,
        event_enqueued_rx: Arc<tokio::sync::Mutex<UnboundedReceiver<()>>>,
        config: Arc<AppConfig>,
        bench_runner: Arc<dyn BenchRunner>,
        octocrab: CachedOctocrab,
    ) -> JoinHandle<anyhow::Result<()>> {
        let last_active_job_id = self.active_job_id.clone();
        let db = self.db.clone();
        let event_enqueued_tx = self.event_enqueued_tx.clone();

        tokio::spawn(async move {
            // When starting up, we need to make sure we will process queued events that are already
            // in the db
            let events = db.queued_event_count().await?;
            for _ in 0..events {
                event_enqueued_tx.send(())?;
            }

            loop {
                // Wait until the next event arrives
                if event_enqueued_rx.lock().await.recv().await.is_none() {
                    break;
                }

                // Now get it from the database
                let event = db.next_queued_event().await?;

                let Some(github_event) = AllowedEvent::from_event_string(&event.event) else {
                    error!(
                        event = event.event,
                        "found and discarded forbidden event in the queue"
                    );

                    db.delete_event(event.id).await?;
                    continue;
                };

                if let Some(job_id) = event.job_id {
                    // It looks like we crashed while handling this event. If the job _did_ finish,
                    // however, we can assume the event was processed but we failed to remove it
                    // from the queue (otherwise we will try to handle it again)
                    let job = db.job(job_id).await?;
                    if job.finished_utc.is_some() {
                        db.delete_event(event.id).await?;
                        continue;
                    }
                }

                let job_id = db.new_job_for_event(event.id, event.created_utc).await?;
                *last_active_job_id.lock().unwrap() = Some(job_id);

                let job_output_dir = config.job_output_dir.join(job_id.to_string());
                let ctx = JobContext {
                    event: &event.event,
                    job_id,
                    job_output_dir,
                    octocrab: &octocrab,
                    event_payload: &event.payload,
                    config: &config,
                    bench_runner: bench_runner.clone(),
                    db: db.clone(),
                };

                match github_event {
                    AllowedEvent::IssueComment => handle_issue_comment(ctx)
                        .await
                        .expect("error handling issue comment"),
                    AllowedEvent::PullRequest => handle_pr_update(ctx)
                        .await
                        .expect("error handling PR update"),
                    AllowedEvent::PullRequestReview => handle_pr_review(ctx)
                        .await
                        .expect("error handling PR review"),
                    AllowedEvent::Push => {
                        bench_main(ctx).await.expect("error handling branch push")
                    }
                };

                db.job_finished(job_id).await?;
                db.delete_event(event.id).await?;
            }

            Ok(())
        })
    }

    /// Enqueue an event.
    ///
    /// Returns `None` if the event kind is not allowed.
    pub async fn enqueue(&self, event: &str, webhook_body: Bytes) -> anyhow::Result<Option<Uuid>> {
        if AllowedEvent::from_event_string(event).is_none() {
            return Ok(None);
        }

        let event_id = self.db.enqueue_event(event, &webhook_body).await.unwrap();
        self.event_enqueued_tx.send(())?;

        Ok(Some(event_id))
    }

    /// Returns the status corresponding to the given job id, or `None` if the job could not be found
    pub async fn job_status(&self, job_id: Uuid) -> anyhow::Result<Option<JobStatus>> {
        let Some(job) = self.db.maybe_job(job_id).await? else {
            return Ok(None);
        };

        let active = *self.active_job_id.lock().unwrap() == Some(job.id);
        Ok(Some(JobStatus { job, active }))
    }
}

/// Allowed GitHub events that we process
#[derive(Copy, Clone)]
pub enum AllowedEvent {
    IssueComment,
    PullRequest,
    PullRequestReview,
    Push,
}

impl AllowedEvent {
    fn from_event_string(event: &str) -> Option<AllowedEvent> {
        let kind = match event {
            "issue_comment" => AllowedEvent::IssueComment,
            "push" => AllowedEvent::Push,
            "pull_request" => AllowedEvent::PullRequest,
            "pull_request_review" => AllowedEvent::PullRequestReview,
            _ => return None,
        };

        Some(kind)
    }
}

/// Wraps job-related information and dependencies
pub struct JobContext<'a> {
    /// The GitHub event that triggered the job
    pub event: &'a str,
    /// The GitHub event's JSON payload
    pub event_payload: &'a [u8],
    /// The job's id
    pub job_id: Uuid,
    /// A directory to which the job can output logs and results
    pub job_output_dir: PathBuf,

    pub config: &'a AppConfig,
    pub octocrab: &'a CachedOctocrab,
    pub bench_runner: Arc<dyn BenchRunner>,
    pub db: Db,
}

#[derive(Serialize)]
pub struct JobStatus {
    pub job: BenchJob,
    pub active: bool,
}
