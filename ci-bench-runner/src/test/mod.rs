use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use ctor::ctor;
use hmac::digest::FixedOutput;
use hmac::{Hmac, Mac};
use reqwest::StatusCode;
use serde_json::json;
use sha2::Sha256;
use sqlx::{Connection, SqliteConnection};
use tempfile::TempDir;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockGuard, MockServer, ResponseTemplate};

use crate::db::{ScenarioDiff, ScenarioKind};
use crate::runner::{BenchRunner, Log};
use crate::{
    server, AppConfig, CommitIdentifier, Db, WEBHOOK_EVENT_HEADER, WEBHOOK_SIGNATURE_HEADER,
};

mod api {
    pub static INSTALLATION: &str = include_str!("data/api_payloads/app_installation.json");

    pub static CREATE_COMMENT: &str = include_str!("data/api_payloads/create_comment.json");
    pub static PULL_REQUEST: &str = include_str!("data/api_payloads/pull_request.json");
}

mod webhook {
    use super::MockGitHub;

    static ISSUE_COMMENT: &str = include_str!("data/webhook_payloads/issue_comment.json");
    static PULL_REQUEST_OPENED: &str =
        include_str!("data/webhook_payloads/pull_request_opened.json");
    static PULL_REQUEST_REVIEW: &str =
        include_str!("data/webhook_payloads/pull_request_review.json");
    static PULL_REQUEST_SYNCHRONIZE: &str =
        include_str!("data/webhook_payloads/pull_request_synchronize.json");
    static PUSH: &str = include_str!("data/webhook_payloads/push.json");

    pub fn push() -> String {
        PUSH.replace("{{repo}}", &MockGitHub::repo_path())
    }

    pub fn comment(comment: &str, action: &str, author_association: &str) -> String {
        ISSUE_COMMENT
            .replace("{{author-association}}", author_association)
            .replace("{{action}}", action)
            .replace("{{comment-body}}", comment)
    }

    pub fn pull_request_opened() -> String {
        PULL_REQUEST_OPENED
            .replace("{{base-repo}}", &MockGitHub::repo_path())
            .replace("{{head-repo}}", &MockGitHub::repo_path())
    }

    pub fn pull_request_synchronized() -> String {
        PULL_REQUEST_SYNCHRONIZE
            .replace("{{base-repo}}", &MockGitHub::repo_path())
            .replace("{{head-repo}}", &MockGitHub::repo_path())
    }

    pub fn pull_request_review() -> String {
        PULL_REQUEST_REVIEW
            .replace("{{base-repo}}", &MockGitHub::repo_path())
            .replace("{{head-repo}}", &MockGitHub::repo_path())
            .replace("{{author-association}}", "OWNER")
    }
}

mod cachegrind {
    pub static SAMPLE_OUTPUT: &str = include_str!("data/cachegrind_outputs/sample");
}

struct MockBenchRunner {
    config: std::sync::Mutex<MockBenchRunnerConfig>,
    runs_tx: UnboundedSender<MockBenchRun>,
    runs: Mutex<UnboundedReceiver<MockBenchRun>>,
}

#[derive(Default)]
struct MockBenchRunnerConfig {
    delay: Option<Duration>,
    crash: bool,
}

struct MockBenchRun {
    commit: CommitIdentifier,
}

impl MockBenchRunner {
    fn new() -> Self {
        let (runs_tx, runs) = tokio::sync::mpsc::unbounded_channel();
        Self {
            config: std::sync::Mutex::new(MockBenchRunnerConfig::default()),
            runs_tx,
            runs: Mutex::new(runs),
        }
    }
}

impl BenchRunner for MockBenchRunner {
    fn checkout_and_run_benchmarks(
        &self,
        commit: &CommitIdentifier,
        _: &Path,
        job_output_dir: &Path,
        _: &mut Vec<Log>,
    ) -> anyhow::Result<()> {
        if self.config.lock().unwrap().crash {
            bail!("bench runner crashed :O");
        }

        // Simulate benchmark duration
        if let Some(duration) = self.config.lock().unwrap().delay {
            std::thread::sleep(duration);
        }

        // Store fake results for the comparison, including cachegrind raw files
        let results_dir = job_output_dir.join("results");
        let cachegrind_dir = results_dir.join("cachegrind");
        fs::create_dir_all(&results_dir)?;
        fs::create_dir(&cachegrind_dir)?;
        fs::write(results_dir.join("icounts.csv"), "fake_bench,12345")?;
        fs::write(
            cachegrind_dir.join("calibration"),
            cachegrind::SAMPLE_OUTPUT,
        )?;
        fs::write(cachegrind_dir.join("fake_bench"), cachegrind::SAMPLE_OUTPUT)?;

        // Notify any watchers of this call
        self.runs_tx
            .send(MockBenchRun {
                commit: commit.clone(),
            })
            .unwrap();
        Ok(())
    }
}

// Poor man's test initialization
#[ctor]
fn setup() {
    // Uncomment to get logs for the tests and have more context when debugging
    // env_logger::builder()
    //     .filter(Some("ci_bench_runner"), tracing::log::LevelFilter::Trace)
    //     .filter(Some("octocrab"), tracing::log::LevelFilter::Trace)
    //     .init();
}

#[tokio::test]
async fn test_issue_comment_unauthorized_user() {
    // Mock HTTP responses from GitHub
    let mock_github = MockGitHub::start().await;

    // Run the job server
    let server = TestServer::start(&mock_github).await;

    // Post the webhook event
    let client = reqwest::Client::default();
    let event = webhook::comment("@rustls-bench bench", "created", "NONE");
    post_webhook(
        &client,
        &server.base_url,
        &server.config.webhook_secret,
        event,
        "issue_comment",
    )
    .await;

    // Ensure the task has already been handled and no requests were made
    ensure_webhook_handled(&server).await;
    mock_github.server.verify().await;
}

#[tokio::test]
async fn test_issue_comment_edited() {
    // Mock HTTP responses from GitHub
    let mock_github = MockGitHub::start().await;

    // Run the job server
    let server = TestServer::start(&mock_github).await;

    // Post the webhook event
    let client = reqwest::Client::default();
    let event = webhook::comment("@rustls-bench bench", "edit", "OWNER");
    post_webhook(
        &client,
        &server.base_url,
        &server.config.webhook_secret,
        event,
        "issue_comment",
    )
    .await;

    // Ensure the task has already been handled and no requests were made
    ensure_webhook_handled(&server).await;
    mock_github.server.verify().await;
}

#[tokio::test]
async fn test_issue_comment_happy_path() {
    // Mock HTTP responses from GitHub
    let mock_github = MockGitHub::start().await;
    let get_pr = mock_github.mock_get_pr().await;
    let post_comment = mock_github.mock_post_comment().await;
    let update_status = mock_github.mock_post_status().await;

    // Run the job server
    let server = TestServer::start(&mock_github).await;

    // Post the webhook event
    let client = reqwest::Client::default();
    let event = webhook::comment("@rustls-bench bench", "created", "OWNER");
    post_webhook(
        &client,
        &server.base_url,
        &server.config.webhook_secret,
        event,
        "issue_comment",
    )
    .await;

    // Wait for our mock endpoints to have been called
    tokio::time::timeout(Duration::from_secs(1), get_pr.wait_until_satisfied())
        .await
        .ok();
    tokio::time::timeout(Duration::from_secs(1), post_comment.wait_until_satisfied())
        .await
        .ok();
    tokio::time::timeout(Duration::from_secs(1), update_status.wait_until_satisfied())
        .await
        .ok();

    // Assert that the mocks were used and report any errors
    mock_github.server.verify().await;
}

#[tokio::test]
async fn test_pr_opened_happy_path_with_comment_reuse() {
    // Mock HTTP responses from GitHub
    let mock_github = MockGitHub::start().await;
    let _post_comment = mock_github.mock_post_comment().await;
    let post_status = mock_github.mock_post_status().await;

    // Run the job server
    let server = TestServer::start(&mock_github).await;

    // Post the webhook event
    let client = reqwest::Client::default();
    post_webhook(
        &client,
        &server.base_url,
        &server.config.webhook_secret,
        webhook::pull_request_opened(),
        "pull_request",
    )
    .await;

    // Wait for our post status endpoint to have been called
    tokio::time::timeout(Duration::from_secs(1), post_status.wait_until_satisfied())
        .await
        .ok();

    // Second webhook event
    let _update_comment = mock_github.mock_update_comment().await;
    drop(post_status);
    let post_status = mock_github.mock_post_status().await;
    post_webhook(
        &client,
        &server.base_url,
        &server.config.webhook_secret,
        webhook::pull_request_opened(),
        "pull_request",
    )
    .await;

    // Wait for our post status endpoint to have been called
    tokio::time::timeout(Duration::from_secs(1), post_status.wait_until_satisfied())
        .await
        .ok();

    // Assert that the mocks were used and report any errors
    mock_github.server.verify().await;
}

#[tokio::test]
async fn test_pr_synchronize_happy_path() {
    // Mock HTTP responses from GitHub
    let mock_github = MockGitHub::start().await;
    let post_comment = mock_github.mock_post_comment().await;
    let post_status = mock_github.mock_post_status().await;

    // Run the job server
    let server = TestServer::start(&mock_github).await;

    // Post the webhook event
    let client = reqwest::Client::default();
    post_webhook(
        &client,
        &server.base_url,
        &server.config.webhook_secret,
        webhook::pull_request_synchronized(),
        "pull_request",
    )
    .await;

    // Wait for our mock endpoints to have been called
    tokio::time::timeout(Duration::from_secs(1), post_comment.wait_until_satisfied())
        .await
        .ok();
    tokio::time::timeout(Duration::from_secs(1), post_status.wait_until_satisfied())
        .await
        .ok();

    // Assert that the mocks were used and report any errors
    mock_github.server.verify().await;
}

#[tokio::test]
async fn test_pr_synchronize_cached() {
    // Mock HTTP responses from GitHub
    let mock_github = MockGitHub::start().await;
    let post_comment = mock_github.mock_post_comment().await;
    let post_status = mock_github.mock_post_status().await;

    // Run the job server
    let server = TestServer::start(&mock_github).await;

    // Ensure the DB already has a stored comparison result for the provided commit pair
    server
        .db
        .store_comparison_result(
            "7edbfb999b352aa09fe669e9103d8155d7e7d890".to_string(),
            "b0b69e925b2c9c6187cb16f361dd36e156f8e097".to_string(),
            Vec::new(),
            vec![ScenarioDiff {
                scenario_name: "foo".to_string(),
                scenario_kind: ScenarioKind::Icount,
                baseline_result: 1000.0,
                candidate_result: 1001.0,
                significance_threshold: 0.35,
                cachegrind_diff: "dummy cachegrind diff".to_string(),
            }],
        )
        .await
        .unwrap();

    // The bench runner will crash if it runs, making the test fail
    server.mock_bench_runner.config.lock().unwrap().crash = true;

    // Post the webhook event
    let client = reqwest::Client::default();
    post_webhook(
        &client,
        &server.base_url,
        &server.config.webhook_secret,
        webhook::pull_request_synchronized(),
        "pull_request",
    )
    .await;

    // Wait for our mock endpoints to have been called
    tokio::time::timeout(Duration::from_secs(1), post_comment.wait_until_satisfied())
        .await
        .ok();
    tokio::time::timeout(Duration::from_secs(1), post_status.wait_until_satisfied())
        .await
        .ok();

    // Assert that the mocks were used and report any errors
    mock_github.server.verify().await;
}

#[tokio::test]
async fn test_pr_review_happy_path() {
    // Mock HTTP responses from GitHub
    let mock_github = MockGitHub::start().await;
    let post_comment = mock_github.mock_post_comment().await;
    let post_status = mock_github.mock_post_status().await;

    // Run the job server
    let server = TestServer::start(&mock_github).await;

    // Post the webhook event
    let client = reqwest::Client::default();
    post_webhook(
        &client,
        &server.base_url,
        &server.config.webhook_secret,
        webhook::pull_request_review(),
        "pull_request_review",
    )
    .await;

    // Wait for our mock endpoints to have been called
    tokio::time::timeout(Duration::from_secs(1), post_comment.wait_until_satisfied())
        .await
        .ok();
    tokio::time::timeout(Duration::from_secs(1), post_status.wait_until_satisfied())
        .await
        .ok();

    // Assert that the mocks were used and report any errors
    mock_github.server.verify().await;
}

#[tokio::test]
async fn test_push_happy_path() {
    // Mock HTTP responses from GitHub
    let mock_github = MockGitHub::start().await;

    // Run the job server
    let server = TestServer::start(&mock_github).await;
    server.mock_bench_runner.config.lock().unwrap().delay = Some(Duration::from_secs(2));

    // Post the webhook event
    let client = reqwest::Client::default();
    let event = webhook::push();
    post_webhook(
        &client,
        &server.base_url,
        &server.config.webhook_secret,
        event,
        "push",
    )
    .await;

    // Ensure the job has been created
    tokio::time::sleep(Duration::from_millis(100)).await;
    let jobs = server.db.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);

    // Post a second webhook event
    let client = reqwest::Client::default();
    let event = webhook::push();
    post_webhook(
        &client,
        &server.base_url,
        &server.config.webhook_secret,
        event,
        "push",
    )
    .await;

    // Ensure no new jobs have been created, but the event has been enqueued
    tokio::time::sleep(Duration::from_millis(100)).await;
    let new_jobs = server.db.jobs().await.unwrap();
    assert_eq!(new_jobs, jobs);
    let events = server.db.queued_events().await.unwrap();
    assert_eq!(events.len(), 2);

    // Check the bench run for the first event
    let expected_clone_url = format!("https://github.com/{}.git", MockGitHub::repo_path());
    let run = tokio::time::timeout(
        Duration::from_secs(3),
        server.mock_bench_runner.runs.lock().await.recv(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(run.commit.branch_name, "main");
    assert_eq!(run.commit.clone_url, expected_clone_url);

    // Check the bench run for the second event
    let run = tokio::time::timeout(
        Duration::from_secs(3),
        server.mock_bench_runner.runs.lock().await.recv(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(run.commit.branch_name, "main");
    assert_eq!(run.commit.clone_url, expected_clone_url);

    // No more runs
    assert!(server
        .mock_bench_runner
        .runs
        .lock()
        .await
        .try_recv()
        .is_err());

    // Ensure the jobs are now listed as finished and their events have been deleted
    tokio::time::sleep(Duration::from_millis(100)).await;
    let jobs = server.db.jobs().await.unwrap();
    assert_eq!(jobs.len(), 2);
    for job in jobs {
        assert!(job.finished_utc.is_some());
    }
    let events = server.db.queued_events().await.unwrap();
    assert!(events.is_empty());

    // Ensure results are stored in the DB
    let result_count = server
        .db
        .result_history(OffsetDateTime::now_utc() - time::Duration::days(2))
        .await
        .unwrap()
        .len();
    assert_eq!(result_count, 2);
}

#[tokio::test]
async fn test_get_cachegrind_diff() {
    let mock_github = MockGitHub::start().await;
    let server = TestServer::start(&mock_github).await;

    // Ensure the DB has a stored comparison result
    server
        .db
        .store_comparison_result(
            "7edbfb999b352aa09fe669e9103d8155d7e7d890".to_string(),
            "b0b69e925b2c9c6187cb16f361dd36e156f8e097".to_string(),
            Vec::new(),
            vec![ScenarioDiff {
                scenario_name: "foo".to_string(),
                scenario_kind: ScenarioKind::Icount,
                baseline_result: 1000.0,
                candidate_result: 1001.0,
                significance_threshold: 0.35,
                cachegrind_diff: "dummy cachegrind diff".to_string(),
            }],
        )
        .await
        .unwrap();

    let client = reqwest::Client::default();

    // Found
    let endpoint = format!("{}/comparisons/7edbfb999b352aa09fe669e9103d8155d7e7d890:b0b69e925b2c9c6187cb16f361dd36e156f8e097/cachegrind-diff/foo", server.base_url);
    let response = client.get(endpoint).send().await.unwrap();
    let status = response.status();
    let body = response.bytes().await.unwrap();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(body, "dummy cachegrind diff");
    assert_eq!(status, StatusCode::OK);

    // Not found
    let endpoint = format!("{}/comparisons/7edbfb999b352aa09fe669e9103d8155d7e7d890:b0b69e925b2c9c6187cb16f361dd36e156f8e097/cachegrind-diff/bar", server.base_url);
    let response = client.get(endpoint).send().await.unwrap();
    let status = response.status();
    let body = response.bytes().await.unwrap();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(
        body,
        "comparison not found for the provided commit hashes and scenario"
    );
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn post_webhook(
    client: &reqwest::Client,
    base_url: &str,
    secret: &str,
    event: String,
    event_kind: &str,
) {
    let signature = sign(secret, event.as_bytes());
    let signature = format!("sha256={}", hex::encode(signature));
    let response = client
        .post(format!("{base_url}/webhooks/github"))
        .header(WEBHOOK_SIGNATURE_HEADER, signature)
        .header(WEBHOOK_EVENT_HEADER, event_kind)
        .body(event)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

async fn ensure_webhook_handled(server: &TestServer) {
    tokio::time::sleep(Duration::from_secs(1)).await;
    let events = server.db.queued_events().await.unwrap();
    assert!(events.is_empty());
    let jobs = server.db.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].finished_utc.is_some());
}

fn installation(installation_id: u64) -> String {
    api::INSTALLATION.replace("{{installation-id}}", &installation_id.to_string())
}

fn pull_request() -> String {
    api::PULL_REQUEST.replace("{{clone_url}}", "https://github.com/rustls/rustls.git")
}

fn sign(secret: &str, payload: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(payload);
    mac.finalize_fixed().to_vec()
}

fn test_config(tmp_path: &Path, github_url: String) -> Arc<AppConfig> {
    Arc::new(AppConfig {
        github_api_url_override: Some(github_url),
        app_base_url: "https://example.com".to_string(),
        job_output_dir: tmp_path.join("logs"),
        path_to_db: ":memory:".to_string(),
        webhook_secret: "secret".to_string(),
        github_app_id: 42,
        github_app_key: dummy_key().to_string(),
        github_repo_owner: MockGitHub::REPO_OWNER.to_string(),
        github_repo_name: MockGitHub::REPO_NAME.to_string(),
        sentry_dsn: "".to_string(),
        port: None,
    })
}

struct MockGitHub {
    server: MockServer,
}

impl MockGitHub {
    const APP_INSTALLATION: u64 = 42;
    const REPO_OWNER: &'static str = "some-owner";
    const REPO_NAME: &'static str = "some-repo";

    fn repo_path() -> String {
        format!("{}/{}", Self::REPO_OWNER, Self::REPO_NAME)
    }

    async fn start() -> Self {
        let server = MockServer::start().await;
        Self { server }
    }

    async fn mock_get_app_installation(&self) -> MockGuard {
        let installation_path = format!("/repos/{}/installation", Self::repo_path());
        let get_app_installation = Mock::given(method("GET"))
            .and(path(&installation_path))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(installation(Self::APP_INSTALLATION)),
            )
            .expect(1);

        self.server.register_as_scoped(get_app_installation).await
    }

    async fn mock_get_access_token(&self) -> MockGuard {
        let expires_at = (OffsetDateTime::now_utc() + time::Duration::days(1))
            .format(&Rfc3339)
            .unwrap();

        let response = json!({
            "token": "foo",
            "expires_at": expires_at,
            "permissions": {},
        })
        .to_string();

        let get_access_token = Mock::given(method("POST"))
            .and(path(format!(
                "/app/installations/{}/access_tokens",
                Self::APP_INSTALLATION
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string(response))
            .expect(1);

        self.server.register_as_scoped(get_access_token).await
    }

    async fn mock_get_pr(&self) -> MockGuard {
        let get_pull_request = Mock::given(method("GET"))
            .and(path_regex(format!(
                r"/repos/{}/pulls/\d+",
                Self::repo_path()
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string(pull_request()))
            .expect(1);

        self.server.register_as_scoped(get_pull_request).await
    }

    async fn mock_post_status(&self) -> MockGuard {
        let response = r#"{ "state": "success" }"#;
        let post_status = Mock::given(method("POST"))
            .and(path_regex(format!(
                "/repos/{}/statuses/[a-f0-9]+",
                Self::repo_path()
            )))
            .respond_with(ResponseTemplate::new(201).set_body_string(response))
            .expect(2);

        self.server.register_as_scoped(post_status).await
    }

    async fn mock_post_comment(&self) -> MockGuard {
        let post_comment = Mock::given(method("POST"))
            .and(path_regex(format!(
                r"/repos/{}/issues/\d+/comments",
                Self::repo_path()
            )))
            .and(body_string_contains(
                "Significant instruction count differences",
            ))
            .respond_with(ResponseTemplate::new(201).set_body_string(api::CREATE_COMMENT))
            .expect(1);

        self.server.register_as_scoped(post_comment).await
    }

    async fn mock_update_comment(&self) -> MockGuard {
        let post_comment = Mock::given(method("POST"))
            .and(path_regex(format!(
                r"/repos/{}/issues/comments/\d+",
                Self::repo_path()
            )))
            .and(body_string_contains(
                "Significant instruction count differences",
            ))
            .respond_with(ResponseTemplate::new(201).set_body_string(api::CREATE_COMMENT))
            .expect(1);

        self.server.register_as_scoped(post_comment).await
    }

    fn url(&self) -> String {
        self.server.uri()
    }
}

struct TestServer {
    _tmp: TempDir,
    config: Arc<AppConfig>,
    base_url: String,
    mock_bench_runner: Arc<MockBenchRunner>,
    db: Db,
}

impl TestServer {
    async fn start(github: &MockGitHub) -> Self {
        // Dependencies
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path(), github.url());
        fs::create_dir(&config.job_output_dir).unwrap();

        let mock_bench_runner = Arc::new(MockBenchRunner::new());

        let sqlite = SqliteConnection::connect("sqlite::memory:").await.unwrap();
        let sqlite = Arc::new(Mutex::new(sqlite));

        // Mock GitHub endpoints related to the app and its installations
        let mock_get_app = github.mock_get_app_installation().await;
        let mock_get_token = github.mock_get_access_token().await;

        let (server, server_addr) =
            server(config.clone(), mock_bench_runner.clone(), sqlite.clone())
                .await
                .unwrap();

        tokio::spawn(server);

        // Sanity check: ensure the server retrieved the app installation and an auth token
        tokio::time::timeout(Duration::from_secs(1), mock_get_app.wait_until_satisfied())
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            mock_get_token.wait_until_satisfied(),
        )
        .await
        .unwrap();

        Self {
            _tmp: tmp,
            config,
            base_url: format!("http://{}:{}", server_addr.ip(), server_addr.port()),
            mock_bench_runner,
            db: Db::with_connection(sqlite),
        }
    }
}

fn dummy_key() -> &'static str {
    r"-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQBiChMZaVedH/mZbnmFIDtabfhVHJd+Nh2aF1CwrYvXFLVED51z
F5XdvF0x6Y3YAGeZaXxVAMDvtnIaHDhfcVQg8mVfBjGTriX5Nt6s05S/AzxEgn/P
BChHhquijwckK07Jef1Y03KE3A+fpCNVCoChYn5fIkaxRWT8WOgdZ8EH8VqXpche
CpIiL9oG2bjxKvILY8tie6lXqxFPM+areedqXYNijGYTQDsH9PuiuiAuZ5DkgMnr
wDaWamTC8FvAgO6/Mn1kyjxScAu7Y7uRhkL2JauNVdkxuY6VtD95M7pcaDGYYu5g
+84HWMTR+bI/1u2Q9n+ZV/qS1cUw3DVLEXX5AgMBAAECggEADGa9178NiCCdUB07
Xe2f1GaIvStqtlpeEDnWySKKx+AktcFL510aZfwHxeKHQMV8VVmUkqQPw8LOWCMt
tlT9kVVYIVcFOmsS/p1EOZRiAm+EVh4z0Jn0BmgwmdWBz79yreWyeGP23nt/tm/q
0D0N3Fw7JAmP66idh5YvdljDgB+M2aRa2E22FNrRWimtGdbZZxI7j/0qP9RIxNLx
V58Dkr1Ja5VWJgsxGo+IsV6QjKZQNCBRMGnsLS7BKYyKhbOuccjb7/lGhBO85bGy
7r+HMvuOxBUJfokfSCc3pMPyyRhvoK5Y26csmDMtc9oHbFqyqlAkGvK9pfo0da85
mnXFAQKBgQC+AocIpDygf5aRmm5wenWT5yyMVqPCn0zjCsK1JT5mxSHyyjas90qF
oX/qVgaU7KG2dMMc9jaTT1pcmjoPuk3CuK5Bq/7XLwuu54rYMAPX5ILTPJEW/o6a
cOX6bSU5GM0HigsCQZPywf9TqRtMEozg5wyuS/Hr6WSb/AcGp4nP6QKBgQCEFpiW
sjDPxcZctnBexkIlE50CVAUKkp2Hel5Z5hMitt6rzgsvhkVoJQhUSFbXzaKfgLWL
VqdToiGvVk0Nwu+J+wzrWEk/yvE1sEEfJuJF70YGlfJ5lPp6EHiNL0XrJvkUpvS/
h/dhvogk6iKq574RM+y1GwmFSGt3JwIK81g7kQKBgQCY+jf1gSU+ovp6p7ca370i
IxD+vBKEcvTYJqW0ahPfcf9vFdcHUuGwzOHLrQ8Hf6yC1WbxPlmaKF08CP+OAhTx
HPdO8EbwwHPLkad7fszZWKTrpOu7c58kQJkoEg/R9GG+HCnY2yteW0pR9OiBSr4Z
pGvVOFfB89qIq1SMyv5tYQKBgDriV+PWTCxT3ro2GqIlgBdHRxdinVy5P8DFrIon
JyCypVGx6QqmsQpcd/oaxZwu7/BrUINtfeqqvJmNv4wC+wZoBLpmAUGPFzj3+hAJ
JZZHtM/6yL2qzH7eGN/X0zOhjCjIxRMdagsJBWhveET4SqMgosWZ6ASi5EWZ/i8j
jJIBAoGBAI8ZcxkoKC8dC8Cmymga+jq6Ic3EuCyCpqdQ0u/lC7fUIlnbCvv2gVWg
2o4GJoHnvdGl+fjL06OMeosxct2I2ExUIgUXtRBzUBjn9V1HvefwtGWv2aRyL1wY
TKxsHhN/F4k8heETaWLH5pcow+QW6ymmtbvgV+y0OOn6HXJOfR9d
-----END RSA PRIVATE KEY-----"
}
