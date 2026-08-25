use std::time::Duration;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use octocrab::Octocrab;
use tokio::time::timeout;

use crate::github::RepoStatus;
use crate::sort::SortMode;
use crate::theme::Theme;
use crate::{config, display, github, github_client, graphql, sort};

const SUBSCRIBED_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run(
    crab: &Octocrab,
    theme: &Theme,
    limit: Option<usize>,
    subscribed_only: bool,
    sort_mode: SortMode,
) -> Result<()> {
    let cfg = config::load()?;

    if cfg.repos.is_empty() {
        println!("No repos tracked. Run `ghpending add` to get started.");
        return Ok(());
    }

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    spinner.set_message("Fetching…");
    spinner.enable_steady_tick(Duration::from_millis(100));

    let subscribed = if subscribed_only {
        Some(
            match timeout(
                SUBSCRIBED_FETCH_TIMEOUT,
                github::fetch_subscribed_items(crab),
            )
            .await
            {
                Ok(Ok(subscribed)) => subscribed,
                Ok(Err(error)) => {
                    spinner.finish_and_clear();
                    return Err(error);
                }
                Err(_) => {
                    spinner.finish_and_clear();
                    anyhow::bail!(
                        "listing subscribed issues and pull requests timed out after 30s"
                    );
                }
            },
        )
    } else {
        None
    };
    let mut results = if use_graphql(github_client::github_token().is_some()) {
        graphql::fetch_repos_batched(crab, &cfg.repos, subscribed.as_ref()).await
    } else {
        github::fetch_repos_rest(crab, &cfg.repos, subscribed.as_ref()).await
    };

    spinner.finish_and_clear();

    sort::sort_results(&mut results, sort_mode);

    let digest = display::render_digest(&results, theme, limit);
    print!("{digest}");

    if all_repo_fetches_failed(&results) {
        anyhow::bail!("all repository fetches failed");
    }

    Ok(())
}

/// GitHub's GraphQL endpoint has no anonymous mode, so without a token we
/// fall back to REST (which does support unauthenticated, rate-limited
/// access) instead of failing every repo fetch outright.
fn use_graphql(has_token: bool) -> bool {
    has_token
}

pub(crate) fn all_repo_fetches_failed(results: &[crate::github::RepoResult]) -> bool {
    !results.is_empty()
        && results
            .iter()
            .all(|result| matches!(result.status, RepoStatus::Error(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{RepoError, RepoResult};

    #[test]
    fn use_graphql_requires_a_token() {
        assert!(use_graphql(true));
        assert!(!use_graphql(false));
    }

    #[test]
    fn all_repo_fetches_failed_requires_every_result_to_be_error() {
        assert!(all_repo_fetches_failed(&[
            RepoResult {
                repo: "a/b".into(),
                status: RepoStatus::Error(RepoError::Timeout),
            },
            RepoResult {
                repo: "c/d".into(),
                status: RepoStatus::Error(RepoError::Api("boom".into())),
            },
        ]));

        assert!(!all_repo_fetches_failed(&[RepoResult {
            repo: "a/b".into(),
            status: RepoStatus::NotFound,
        }]));

        assert!(!all_repo_fetches_failed(&[RepoResult {
            repo: "a/b".into(),
            status: RepoStatus::Items(vec![]),
        }]));

        assert!(!all_repo_fetches_failed(&[]));
    }
}
