use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use bencher_client::BencherClient;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{error, info, trace_span, Instrument};
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
    /// Bencher API client
    bencher_client: BencherClient,
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
        let bencher_client = bencher_client::BencherClient::builder()
            .token(config.bencher_api_token.clone())
            .build();
        let queue = Self {
            active_job_id,
            event_enqueued_tx: worker_tx,
            db,
            bencher_client,
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
        let active_job_id = self.active_job_id.clone();
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

                *active_job_id.lock().unwrap() = None;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    /// Spawns a tokio task to process queued events in the background.
    ///
    /// The task might unexpectedly crash. Therefore, it should be supervised and restarted upon
    /// need.
    fn process_queued_events_in_background(
        &self,
        event_enqueued_rx: Arc<tokio::sync::Mutex<UnboundedReceiver<()>>>,
        config: Arc<AppConfig>,
        bench_runner: Arc<dyn BenchRunner>,
        octocrab: CachedOctocrab,
    ) -> JoinHandle<anyhow::Result<()>> {
        let active_job_id = self.active_job_id.clone();
        let db = self.db.clone();
        let event_enqueued_tx = self.event_enqueued_tx.clone();
        let bencher_client = self.bencher_client.clone();

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

                if event.job_id.is_some() {
                    // It looks like we crashed while handling this event. Let's remove it from the
                    // queue to avoid an infinite crash loop.
                    db.delete_event(event.id).await?;
                }

                let job_id = db.new_job_for_event(event.id, event.created_utc).await?;
                *active_job_id.lock().unwrap() = Some(job_id);

                let span = trace_span!(
                    "handle event",
                    job_id = job_id.to_string(),
                    event = event.event
                );
                async {
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
                        bencher_client: &bencher_client,
                    };

                    let result = match github_event {
                        AllowedEvent::IssueComment => handle_issue_comment(ctx).await,
                        AllowedEvent::PullRequest => handle_pr_update(ctx).await,
                        AllowedEvent::PullRequestReview => handle_pr_review(ctx).await,
                        AllowedEvent::Push => bench_main(ctx).await,
                    };

                    if let Err(e) = &result {
                        error!(
                            cause = e.to_string(),
                            "error handling event: {github_event:?}"
                        );
                    }

                    db.job_finished(job_id, result.is_ok()).await?;
                    db.delete_event(event.id).await?;

                    Ok::<_, anyhow::Error>(())
                }
                .instrument(span)
                .await?
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

    /// Returns a user-facing view of the given job id, or `None` if the job could not be found
    pub async fn job_view(&self, job_id: Uuid) -> anyhow::Result<Option<JobView>> {
        let Some(job) = self.db.maybe_job(job_id).await? else {
            return Ok(None);
        };

        let active = *self.active_job_id.lock().unwrap() == Some(job_id);
        Ok(Some(JobView::from_job(job, active)))
    }
}

/// Allowed GitHub events that we process
#[derive(Copy, Clone, Debug)]
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
    pub bencher_client: &'a BencherClient,
}

#[derive(Serialize, Deserialize)]
pub struct JobView {
    #[serde(with = "time::serde::rfc3339")]
    pub created_utc: OffsetDateTime,
    #[serde(
        serialize_with = "time::serde::rfc3339::option::serialize",
        deserialize_with = "time::serde::rfc3339::option::deserialize"
    )]
    pub finished_utc: Option<OffsetDateTime>,
    pub status: JobStatus,
}

impl JobView {
    fn from_job(job: BenchJob, active: bool) -> Self {
        let status = match job.success {
            None => match &job.finished_utc {
                None if active => JobStatus::Pending,
                None => JobStatus::Failure,
                Some(_) => unreachable!("if the job finished, it's success field will be set"),
            },
            Some(true) => JobStatus::Success,
            Some(false) => JobStatus::Failure,
        };

        Self {
            created_utc: job.created_utc,
            finished_utc: job.finished_utc,
            status,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum JobStatus {
    Pending,
    Success,
    Failure,
}
