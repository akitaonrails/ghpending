use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use octocrab::Octocrab;
use tokio::time::{self, timeout};

use crate::github::{RepoError, RepoResult, RepoStatus, SubscribedItems};
use crate::sort::SortMode;
use crate::theme::Theme;
use crate::{config, display, github, sort};

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONCURRENT_FETCHES: usize = 4;

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
            match timeout(FETCH_TIMEOUT, github::fetch_subscribed_items(crab)).await {
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
    let mut results = fetch_repos(crab, &cfg.repos, subscribed.as_ref()).await;

    spinner.finish_and_clear();

    sort::sort_results(&mut results, sort_mode);

    let digest = display::render_digest(&results, theme, limit);
    print!("{digest}");

    if all_repo_fetches_failed(&results) {
        anyhow::bail!("all repository fetches failed");
    }

    Ok(())
}

async fn fetch_repos(
    crab: &Octocrab,
    repos: &[String],
    subscribed: Option<&SubscribedItems>,
) -> Vec<RepoResult> {
    let empty_subscriptions = HashSet::new();
    let mut results = vec![None; repos.len()];
    let mut in_flight = FuturesUnordered::new();
    let mut next = 0;

    while next < repos.len() && in_flight.len() < MAX_CONCURRENT_FETCHES {
        let repo = repos[next].clone();
        let repo_key = repo.to_ascii_lowercase();
        let subscribed_numbers =
            subscribed.map(|items| items.get(&repo_key).unwrap_or(&empty_subscriptions));
        in_flight.push(fetch_repo_with_timeout(
            crab,
            next,
            repo,
            subscribed_numbers,
        ));
        next += 1;
    }

    let deadline = time::sleep(FETCH_TIMEOUT);
    tokio::pin!(deadline);

    while !in_flight.is_empty() {
        tokio::select! {
            _ = &mut deadline => break,
            Some((index, result)) = in_flight.next() => {
                results[index] = Some(result);

                if next < repos.len() {
                    let repo = repos[next].clone();
                    let repo_key = repo.to_ascii_lowercase();
                    let subscribed_numbers = subscribed
                        .map(|items| items.get(&repo_key).unwrap_or(&empty_subscriptions));
                    in_flight.push(fetch_repo_with_timeout(
                        crab,
                        next,
                        repo,
                        subscribed_numbers,
                    ));
                    next += 1;
                }
            }
        }
    }

    results
        .into_iter()
        .enumerate()
        .map(|(index, result)| result.unwrap_or_else(|| timeout_result(repos[index].clone())))
        .collect()
}

async fn fetch_repo_with_timeout(
    crab: &Octocrab,
    index: usize,
    repo: String,
    subscribed_numbers: Option<&std::collections::HashSet<u64>>,
) -> (usize, RepoResult) {
    let result = match timeout(
        FETCH_TIMEOUT,
        github::fetch_repo_items(crab, &repo, subscribed_numbers),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => timeout_result(repo),
    };
    (index, result)
}

fn timeout_result(repo: String) -> RepoResult {
    RepoResult {
        repo,
        status: RepoStatus::Error(RepoError::Timeout),
    }
}

pub(crate) fn all_repo_fetches_failed(results: &[RepoResult]) -> bool {
    !results.is_empty()
        && results
            .iter()
            .all(|result| matches!(result.status, RepoStatus::Error(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
