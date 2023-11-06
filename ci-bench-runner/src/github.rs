use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use hmac::{Hmac, Mac};
use jsonwebtoken::EncodingKey;
use octocrab::models::{InstallationId, StatusState};
use octocrab::Octocrab;
use sha2::Sha256;
use tracing::{error, trace, warn};

use crate::AppConfig;

pub mod api {
    //! Types used to deserialize responses from the GitHub API in cases where Octocrab does not
    //! provide the necessary fields

    use octocrab::models::pulls::PullRequest;
    use serde::Deserialize;

    #[derive(Debug, Clone, PartialEq, Deserialize)]
    pub struct PullRequestReviewEvent {
        pub action: String,
        pub review: Review,
        pub pull_request: PullRequest,
    }

    #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
    pub struct Review {
        pub author_association: String,
        pub state: String,
        pub commit_id: String,
    }

    #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
    pub struct PushEvent {
        #[serde(rename = "ref")]
        pub git_ref: String,
        pub repository: Repo,
        pub after: String,
        pub deleted: bool,
    }

    #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
    pub struct CommentEvent {
        pub action: String,
        pub comment: Comment,
        pub issue: Issue,
    }

    #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
    pub struct Comment {
        pub author_association: String,
        pub body: String,
        pub user: GitHubUser,
    }

    #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
    pub struct Issue {
        pub number: u64,
        pub pull_request: Option<PullRequestLite>,
    }

    #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
    pub struct PullRequestLite {
        pub url: String,
    }

    #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
    pub struct GitHubUser {
        pub login: String,
        pub id: u64,
    }

    #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
    pub struct Repo {
        pub clone_url: String,
    }
}

/// Provides access to an authenticated `Octocrab` client
#[derive(Debug, Clone)]
pub struct CachedOctocrab {
    /// Initial GitHub client, authenticated as our GitHub App
    app_client: Octocrab,
    /// The installation id corresponding to the repository that has installed the GitHub App
    installation_id: InstallationId,
    /// Regularly refreshed GitHub client, authenticated as an installation of our GitHub app
    ///
    /// This is the client that should be used when interacting with the repository (e.g. posting
    /// comments, setting commit statuses, etc)
    installation_client: Arc<Mutex<Octocrab>>,
}

impl CachedOctocrab {
    /// Creates a new [`CachedOctocrab`] instance.
    ///
    /// This constructor queries the GitHub API to obtain the app installation id and a fresh token
    /// corresponding to it. It also spawns a background tokio task that regularly refreshes
    /// the token.
    pub async fn new(config: &AppConfig) -> anyhow::Result<Self> {
        let key = EncodingKey::from_rsa_pem(config.github_app_key.as_bytes())
            .context("error parsing GitHub App key")?;
        let mut octocrab_builder = Octocrab::builder().app(config.github_app_id.into(), key);

        if let Some(base_url) = &config.github_api_url_override {
            octocrab_builder = octocrab_builder.base_uri(base_url).context("invalid url")?;
        }

        let app_client = octocrab_builder.build()?;
        let installation = app_client
            .apps()
            .get_repository_installation(&config.github_repo_owner, &config.github_repo_name)
            .await
            .context("failed to obtain GitHub app installation information")?;

        let unauthenticated_client = app_client.clone();
        let cache = Self {
            app_client,
            installation_id: installation.id,
            installation_client: Arc::new(Mutex::new(unauthenticated_client)),
        };

        // Obtain an authenticated client for the first time
        cache.refresh_token().await?;

        // Launch the background process to refresh the client in the background
        cache.clone().refresh_token_in_background();

        Ok(cache)
    }

    /// Refresh the installation token once
    async fn refresh_token(&self) -> anyhow::Result<()> {
        let (authenticated_client, _) = self
            .app_client
            .installation_and_token(self.installation_id)
            .await
            .context("failed to obtain GitHub app installation token")?;

        *self.installation_client.lock().unwrap() = authenticated_client;

        Ok(())
    }

    /// Regularly refresh the installation token in the background
    fn refresh_token_in_background(self) {
        tokio::spawn(async move {
            loop {
                // The installation access token will expire after 1 hour, so we refresh it every
                // 30 minutes
                // (see https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation)
                tokio::time::sleep(Duration::from_secs(60 * 30)).await;

                // In case of failure, log it and retry every 5 minutes
                while let Err(e) = self.refresh_token().await {
                    warn!(cause = e.to_string(), "failed to refresh GitHub token");
                    tokio::time::sleep(Duration::from_secs(60 * 5)).await;
                }
            }
        });
    }

    /// Returns the cached and authenticated `Octocrab` client
    pub fn cached(&self) -> Octocrab {
        self.installation_client.lock().unwrap().clone()
    }
}

/// Updates a commit's status and logs the result
pub async fn update_commit_status(
    sha: String,
    state: StatusState,
    job_url: String,
    config: &AppConfig,
    octocrab: &Octocrab,
) {
    let post_result = octocrab
        .repos(&config.github_repo_owner, &config.github_repo_name)
        .create_status(sha, state)
        .context("icount benchmarks".to_string())
        .target(job_url)
        .send()
        .await;

    match post_result {
        Ok(_) => trace!("commit status updated to {state:?}"),
        Err(e) => error!(cause = e.to_string(), "error updating status to {state:?}"),
    }
}

/// Truncates a comment if it exceeds GitHub's size limit
pub fn maybe_truncate_comment(body: &mut String) {
    const GITHUB_COMMENT_MAX_LEN: usize = 65536;

    if body.len() > GITHUB_COMMENT_MAX_LEN {
        let prepend = format!("_Note: the comment has been truncated to respect GitHub's size limit of {GITHUB_COMMENT_MAX_LEN} bytes._");
        body.truncate(GITHUB_COMMENT_MAX_LEN - prepend.len());
        body.insert_str(0, &prepend);
    }
}

/// Returns true if the webhook signature is valid
pub fn verify_webhook_signature(body: &[u8], signature: &str, secret: &str) -> bool {
    // Signatures always start with sha256=
    let signature = &signature[7..];
    let Ok(signature_bytes) = hex::decode(signature) else {
        return false;
    };

    // Safe to unwrap because any key is valid
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    mac.verify_slice(signature_bytes.as_slice()).is_ok()
}

#[cfg(test)]
mod test {
    use super::api::*;

    #[test]
    fn parse_comment_created_without_pr_event() {
        let payload = include_str!("test/data/webhook_payloads/issue_comment_without_pr.json");
        let parsed: CommentEvent = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.action, "created");
        assert_eq!(parsed.comment.body, "Comment 1");
        assert_eq!(parsed.comment.author_association, "OWNER");
        assert_eq!(parsed.issue.number, 4);
        assert!(parsed.issue.pull_request.is_none());
    }

    #[test]
    fn parse_comment_created_event() {
        let payload = include_str!("test/data/webhook_payloads/issue_comment.json");
        let parsed: CommentEvent = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.action, "{{action}}");
    }

    #[test]
    fn parse_push_event() {
        let payload = include_str!("test/data/webhook_payloads/push.json");
        let parsed: PushEvent = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.git_ref, "refs/heads/main");
        assert!(!parsed.deleted);
    }
}
