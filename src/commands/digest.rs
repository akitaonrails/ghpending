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
    let mut cfg = config::load()?;

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
    let (mut results, forks_to_persist) = if use_graphql(github_client::github_token().is_some()) {
        let (results, fork_map) =
            graphql::fetch_repos_batched(crab, &cfg.repos, subscribed.as_ref(), &cfg.forks).await;
        (results, merge_fork_cache(&cfg.forks, fork_map))
    } else {
        let (results, detected) =
            github::fetch_repos_rest(crab, &cfg.repos, subscribed.as_ref(), &cfg.forks).await;
        (results, merge_fork_cache(&cfg.forks, detected))
    };

    spinner.finish_and_clear();

    if let Some(forks) = forks_to_persist {
        cfg.forks = forks;
        // Best-effort: the fork cache is just an optimization, so a save
        // failure (e.g. a read-only config dir) shouldn't block the digest.
        let _ = config::save(&cfg);
    }

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

/// Merges freshly observed fork-cache entries over the existing cache and
/// returns the result only when it actually changes something. Merging (not
/// replacing) keeps entries for repos that errored this run, and the fresh
/// values already have manual overrides applied by the fetch layer.
fn merge_fork_cache(
    existing: &crate::github::ForkCache,
    fresh: crate::github::ForkCache,
) -> Option<crate::github::ForkCache> {
    if fresh.is_empty() {
        return None;
    }
    let mut merged = existing.clone();
    merged.extend(fresh);
    if merged == *existing {
        None
    } else {
        Some(merged)
    }
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
            RepoResult::new("a/b".into(), RepoStatus::Error(RepoError::Timeout)),
            RepoResult::new(
                "c/d".into(),
                RepoStatus::Error(RepoError::Api("boom".into()))
            ),
        ]));

        assert!(!all_repo_fetches_failed(&[RepoResult::new(
            "a/b".into(),
            RepoStatus::NotFound
        )]));

        assert!(!all_repo_fetches_failed(&[RepoResult::new(
            "a/b".into(),
            RepoStatus::Items(vec![])
        )]));

        assert!(!all_repo_fetches_failed(&[]));
    }

    #[test]
    fn merge_fork_cache_preserves_existing_and_detects_no_change() {
        let mut existing = crate::github::ForkCache::new();
        existing.insert("a/errored-this-run".into(), "up/a".into());
        existing.insert("b/opted-out".into(), String::new());

        let mut fresh = crate::github::ForkCache::new();
        fresh.insert("b/opted-out".into(), String::new());
        fresh.insert("c/new-fork".into(), "up/c".into());

        let merged = merge_fork_cache(&existing, fresh.clone()).expect("changed");
        assert_eq!(merged.get("a/errored-this-run").unwrap(), "up/a");
        assert_eq!(merged.get("b/opted-out").unwrap(), "");
        assert_eq!(merged.get("c/new-fork").unwrap(), "up/c");

        assert!(merge_fork_cache(&merged, fresh).is_none());
        assert!(merge_fork_cache(&existing, crate::github::ForkCache::new()).is_none());
    }
}
