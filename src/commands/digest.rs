use std::time::Duration;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use octocrab::Octocrab;
use tokio::time::timeout;

use crate::github::{RepoResult, RepoStatus};
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

    // $GHPENDING_REPOS overrides which repos this run fetches, without ever
    // touching `cfg.repos` itself: `cfg` is passed to `config::save()` below
    // to persist the fork-detection cache, and if the override leaked into
    // `cfg.repos` that save would silently overwrite the tracked list on
    // disk with the one-off override.
    let repos = config::repos_override_from_env().unwrap_or_else(|| cfg.repos.clone());

    if repos.is_empty() {
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
            graphql::fetch_repos_batched(crab, &repos, subscribed.as_ref(), &cfg.forks).await;
        (results, merge_fork_cache(&cfg.forks, fork_map))
    } else {
        let (results, detected) =
            github::fetch_repos_rest(crab, &repos, subscribed.as_ref(), &cfg.forks).await;
        (results, merge_fork_cache(&cfg.forks, detected))
    };

    spinner.finish_and_clear();

    if let Some(forks) = forks_to_persist {
        cfg.forks = forks;
        // Best-effort: the fork cache is just an optimization, so a save
        // failure (e.g. a read-only config dir) shouldn't block the digest.
        let _ = config::save(&cfg);
    }

    let viewer = resolve_viewer(crab, &cfg).await;
    mark_and_order_items(&mut results, viewer.as_deref());

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

/// Resolves who "the user" is for this digest run: the authenticated login
/// when a token is present and the lookup succeeds, otherwise the configured
/// `user`. A lookup error (or a 401 mapped to `None` by
/// `authenticated_login`) never fails the digest — it just falls back.
async fn resolve_viewer(crab: &Octocrab, cfg: &config::Config) -> Option<String> {
    if github_client::github_token().is_some()
        && let Ok(Some(login)) = github::authenticated_login(crab).await
    {
        return Some(login);
    }
    cfg.user.clone()
}

/// For every non-fork repo with items: tags each item `mine` when its author
/// matches `viewer` (case-insensitive), then stably reorders so others'
/// items list above the viewer's own, preserving the existing PR-first/
/// number-descending order within each group. Fork results (upstream items)
/// are left untouched — that view's ordering is unrelated to "mine".
pub(crate) fn mark_and_order_items(results: &mut [RepoResult], viewer: Option<&str>) {
    for result in results.iter_mut() {
        if result.upstream.is_some() {
            continue;
        }
        if let RepoStatus::Items(items) = &mut result.status {
            for item in items.iter_mut() {
                item.mine = viewer.is_some_and(|v| v.eq_ignore_ascii_case(&item.author));
            }
            items.sort_by_key(|item| item.mine);
        }
    }
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
    use crate::github::{ItemKind, RepoError, RepoItem, RepoResult};

    fn item(number: u64, author: &str) -> RepoItem {
        RepoItem {
            kind: ItemKind::Issue,
            number,
            title: format!("item {number}"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            author: author.into(),
            pr_draft: None,
            comments: None,
            review_decision: None,
            mine: false,
        }
    }

    #[test]
    fn mark_and_order_items_puts_others_above_mine_stably_and_skips_forks() {
        let mut results = vec![
            RepoResult::new(
                "a/b".into(),
                RepoStatus::Items(vec![
                    item(1, "viewer"),
                    item(2, "alice"),
                    item(3, "viewer"),
                    item(4, "bob"),
                ]),
            ),
            RepoResult::new(
                "fork/repo".into(),
                RepoStatus::Items(vec![item(5, "viewer")]),
            )
            .with_upstream("upstream/repo".into()),
        ];

        mark_and_order_items(&mut results, Some("viewer"));

        let RepoStatus::Items(items) = &results[0].status else {
            panic!("expected items")
        };
        let numbers: Vec<u64> = items.iter().map(|i| i.number).collect();
        // Others (alice, bob) keep their relative order above the viewer's
        // own (also kept in relative order) — a stable sort on `mine`.
        assert_eq!(numbers, vec![2, 4, 1, 3]);
        assert!(!items[0].mine);
        assert!(!items[1].mine);
        assert!(items[2].mine);
        assert!(items[3].mine);

        // Fork result untouched, even though its item's author matches the
        // viewer: order and `mine` are left alone.
        let RepoStatus::Items(fork_items) = &results[1].status else {
            panic!("expected items")
        };
        assert_eq!(fork_items[0].number, 5);
        assert!(!fork_items[0].mine);
    }

    #[test]
    fn mark_and_order_items_with_no_viewer_marks_nothing_mine() {
        let mut results = vec![RepoResult::new(
            "a/b".into(),
            RepoStatus::Items(vec![item(1, "alice"), item(2, "bob")]),
        )];

        mark_and_order_items(&mut results, None);

        let RepoStatus::Items(items) = &results[0].status else {
            panic!("expected items")
        };
        assert!(items.iter().all(|i| !i.mine));
    }

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
