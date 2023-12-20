use std::fmt::{Debug, Formatter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use axum::body::Bytes;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{error, info, trace_span, Instrument};
use uuid::Uuid;

use crate::bencher_dev::BencherDev;
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
    /// Keeps track of whether incoming events should be processed.
    ///
    /// Note: when event processing gets disabled, we still let the currently active job run to
    /// completion.
    process_events_toggler: ProcessEventsToggler,
    /// Database handle, used to persist events and recover in case of crashes
    db: Db,
    /// Bencher.dev client
    bencher_dev: Option<BencherDev>,
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
    ) -> anyhow::Result<Self> {
        let (worker_tx, event_enqueued_rx) = tokio::sync::mpsc::unbounded_channel();

        let queue = Self {
            active_job_id: Arc::new(Mutex::new(None)),
            event_enqueued_tx: worker_tx,
            process_events_toggler: ProcessEventsToggler::new()
                .context("failed to initialize ProcessEventsToggler")?,
            db,
            bencher_dev: config.bencher.clone().map(BencherDev::new),
        };

        Ok(queue.start_and_supervise_queue_processing(
            event_enqueued_rx,
            config,
            bench_runner,
            octocrab,
        ))
    }

    /// Starts and supervises the background queue processing task
    fn start_and_supervise_queue_processing(
        self,
        event_enqueued_rx: UnboundedReceiver<()>,
        config: Arc<AppConfig>,
        bench_runner: Arc<dyn BenchRunner>,
        octocrab: CachedOctocrab,
    ) -> Self {
        let active_job_id = self.active_job_id.clone();
        let queue = self.clone();
        let event_enqueued_rx = Arc::new(tokio::sync::Mutex::new(event_enqueued_rx));
        let toggler = self.process_events_toggler.clone();

        tokio::spawn(async move {
            loop {
                let background_task = queue.process_queued_events_in_background(
                    event_enqueued_rx.clone(),
                    config.clone(),
                    toggler.clone(),
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
                            "job queue background task {active_job_id:?} errored, restarting in 1s"
                        );
                    }
                    Err(e) => {
                        // The task panicked or was cancelled
                        error!(
                            cause = e.to_string(),
                            "job queue background task {active_job_id:?} crashed, restarting in 1s"
                        );
                    }
                }

                *active_job_id.lock().unwrap() = None;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        self
    }

    /// Spawns a tokio task to process queued events in the background.
    ///
    /// The task might unexpectedly crash. Therefore, it should be supervised and restarted upon
    /// need.
    fn process_queued_events_in_background(
        &self,
        event_enqueued_rx: Arc<tokio::sync::Mutex<UnboundedReceiver<()>>>,
        config: Arc<AppConfig>,
        mut toggler: ProcessEventsToggler,
        bench_runner: Arc<dyn BenchRunner>,
        octocrab: CachedOctocrab,
    ) -> JoinHandle<anyhow::Result<()>> {
        let active_job_id = self.active_job_id.clone();
        let db = self.db.clone();
        let event_enqueued_tx = self.event_enqueued_tx.clone();
        let bencher_dev = self.bencher_dev.clone();

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

                // Postpone event processing if requested
                toggler.wait_for_processing_enabled().await;

                // Get the next event from the database
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
                        bencher_dev: bencher_dev.as_ref(),
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

    /// Returns the active job's id, if there is an active job
    pub fn active_job_id(&self) -> Option<Uuid> {
        *self.active_job_id.lock().unwrap()
    }

    /// Returns whether event processing is currently enabled
    pub fn event_processing_enabled(&self) -> bool {
        self.process_events_toggler.processing_enabled()
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
    fn from_event_string(event: &str) -> Option<Self> {
        Some(match event {
            "issue_comment" => Self::IssueComment,
            "push" => Self::Push,
            "pull_request" => Self::PullRequest,
            "pull_request_review" => Self::PullRequestReview,
            _ => return None,
        })
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
    pub bencher_dev: Option<&'a BencherDev>,
    pub bench_runner: Arc<dyn BenchRunner>,
    pub db: Db,
}

impl<'a> Debug for JobContext<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobContext")
            .field("event", &self.event)
            .field("job_id", &self.job_id)
            .field("job_output_dir", &self.job_output_dir)
            .finish()
    }
}

#[derive(Debug, Serialize, Deserialize)]
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

/// Watches the filesystem to toggle event processing.
///
/// Event processing is enabled by default, but can be disabled by creating a file called `pause`
/// in the program's working directory. If the file is present upon startup or gets created while
/// the application runs, processing will be disabled until the file is deleted.
struct ProcessEventsToggler {
    /// Filesystem watcher
    watcher: Arc<RecommendedWatcher>,
    /// Receiver tracking the current state of the toggle (enabled / disabled)
    processing_enabled_rx: tokio::sync::watch::Receiver<bool>,
}

impl ProcessEventsToggler {
    fn new() -> anyhow::Result<Self> {
        let pause_file_path = Path::new("pause");
        let process_events = pause_file_path.try_exists().ok() != Some(true);
        let (processing_enabled_tx, processing_enabled_rx) =
            tokio::sync::watch::channel(process_events);

        let mut watcher =
            notify::recommended_watcher(move |r: notify::Result<notify::Event>| match r {
                Ok(event) => {
                    if event
                        .paths
                        .iter()
                        .any(|p| p.file_name() == Some(pause_file_path.as_os_str()))
                    {
                        match event.kind {
                            EventKind::Create(_) => {
                                info!("event processing disabled");
                                processing_enabled_tx.send_replace(false);
                            }
                            EventKind::Remove(_) => {
                                info!("event processing enabled");
                                processing_enabled_tx.send_replace(true);
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => error!("error watching pause file: {:?}", e),
            })?;

        let watched_path = Path::new(".");
        info!(
            "watching for file creation/deletion under {}",
            watched_path.canonicalize()?.display()
        );
        watcher.watch(watched_path, RecursiveMode::NonRecursive)?;

        Ok(ProcessEventsToggler {
            watcher: Arc::new(watcher),
            processing_enabled_rx,
        })
    }

    fn processing_enabled(&self) -> bool {
        *self.processing_enabled_rx.borrow()
    }

    fn processing_disabled(&self) -> bool {
        !self.processing_enabled()
    }

    async fn wait_for_processing_enabled(&mut self) {
        if self.processing_disabled() {
            info!("event handling postponed until processing gets enabled");
        }

        self.processing_enabled_rx.wait_for(|&b| b).await.unwrap();
        assert!(*self.processing_enabled_rx.borrow());
    }
}

impl Clone for ProcessEventsToggler {
    fn clone(&self) -> Self {
        Self {
            watcher: self.watcher.clone(),
            processing_enabled_rx: self.processing_enabled_rx.clone(),
        }
    }
}
