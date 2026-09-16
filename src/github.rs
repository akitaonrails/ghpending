use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures::stream::{FuturesUnordered, StreamExt};
use octocrab::Octocrab;
use serde::Deserialize;
use thiserror::Error;
use tokio::time::{self, timeout};

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONCURRENT_FETCHES: usize = 4;

#[derive(Debug, Clone)]
pub struct RepoItem {
    pub kind: ItemKind,
    pub number: u64,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub author: String,
    pub pr_draft: Option<bool>,
    pub comments: Option<u64>,
    pub review_decision: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ItemKind {
    PullRequest,
    Issue,
}

pub type SubscribedItems = HashMap<String, HashSet<u64>>;

/// Tracked fork name -> upstream name (e.g. `"akitaonrails/omarchy"` ->
/// `"omacom/omarchy"`), auto-managed cache persisted in the config. An empty
/// string value means "checked, confirmed not a fork" (as opposed to a
/// missing entry, which means "unknown, needs checking").
pub type ForkCache = HashMap<String, String>;

#[derive(Debug, Clone)]
pub struct RepoResult {
    pub repo: String,
    pub status: RepoStatus,
    /// `Some(parent)` when `repo` is a fork and `status` holds the items the
    /// user authored on the upstream `parent` repo instead of the fork's own
    /// (usually empty) items.
    pub upstream: Option<String>,
}

impl RepoResult {
    pub fn new(repo: String, status: RepoStatus) -> Self {
        RepoResult {
            repo,
            status,
            upstream: None,
        }
    }

    pub fn with_upstream(mut self, upstream: String) -> Self {
        self.upstream = Some(upstream);
        self
    }
}

#[derive(Debug, Clone)]
pub enum RepoStatus {
    Items(Vec<RepoItem>),
    NotFound,
    Error(RepoError),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RepoError {
    #[error("timeout after 30s")]
    Timeout,
    #[error("{0}")]
    Api(String),
}

#[derive(Debug, Error)]
pub enum GithubError {
    #[error("repo not found: {0}")]
    NotFound(String),
    #[error("api error: {0}")]
    Api(#[from] octocrab::Error),
}

/// Maps an octocrab result to `GithubError`, treating HTTP 404 as `NotFound`.
fn map_github_err<T>(
    res: std::result::Result<T, octocrab::Error>,
    repo_label: &str,
) -> std::result::Result<T, GithubError> {
    match res {
        Ok(v) => Ok(v),
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code.as_u16() == 404 => {
            Err(GithubError::NotFound(repo_label.to_owned()))
        }
        Err(e) => Err(GithubError::Api(e)),
    }
}

/// Turns an octocrab error into a message with real detail instead of
/// octocrab's `Display`, which collapses `Error::GitHub` down to just
/// "GitHub".
pub(crate) fn describe_api_error(e: &octocrab::Error) -> String {
    match e {
        octocrab::Error::GitHub { source, .. } => {
            let mut message = format!("HTTP {} {}", source.status_code, source.message);
            if source.status_code.as_u16() == 401 {
                message.push_str(" — set GITHUB_TOKEN");
            }
            message
        }
        other => other.to_string(),
    }
}

pub fn split_repo(s: &str) -> Option<(&str, &str)> {
    let (owner, name) = s.split_once('/')?;

    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some((owner, name))
}

/// Whether a GitHub account is a personal user or an organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountKind {
    User,
    Organization,
}

/// Where `add` should pull the candidate repo list from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListSource {
    /// Everything the token can reach (owned + collaborator + org member),
    /// private included. Used for `--all` and for listing your own account.
    Authenticated,
    /// A specific org's repos (private included when the token is a member).
    Org(String),
    /// A third-party user's public repos — all we can see for someone else.
    PublicUser(String),
}

/// Maps the `type` field of a GitHub account profile to an `AccountKind`.
/// Anything that is not exactly "Organization" is treated as a user.
pub fn account_kind_from_type(profile_type: &str) -> AccountKind {
    if profile_type == "Organization" {
        AccountKind::Organization
    } else {
        AccountKind::User
    }
}

/// Decides which `ListSource` `add` should use.
///
/// - `all`: the `--all` flag was passed.
/// - `username`: the resolved target (`None` when `--all`).
/// - `auth_login`: the login the token authenticates as.
/// - `kind`: whether `username` is a user or org (`None` when `--all`).
pub fn resolve_list_source(
    all: bool,
    username: Option<&str>,
    auth_login: &str,
    kind: Option<AccountKind>,
) -> ListSource {
    if all {
        return ListSource::Authenticated;
    }
    let Some(username) = username else {
        return ListSource::Authenticated;
    };
    if username.eq_ignore_ascii_case(auth_login) {
        return ListSource::Authenticated;
    }
    match kind {
        Some(AccountKind::Organization) => ListSource::Org(username.to_owned()),
        _ => ListSource::PublicUser(username.to_owned()),
    }
}

pub fn item_cmp(a: &RepoItem, b: &RepoItem) -> Ordering {
    match (&a.kind, &b.kind) {
        (ItemKind::PullRequest, ItemKind::Issue) => Ordering::Less,
        (ItemKind::Issue, ItemKind::PullRequest) => Ordering::Greater,
        _ => b.number.cmp(&a.number),
    }
}

pub async fn list_user_repos(crab: &Octocrab, username: &str) -> Result<Vec<String>> {
    let first_page = crab
        .users(username)
        .repos()
        .r#type(octocrab::params::users::repos::Type::Owner)
        .per_page(100)
        .send()
        .await
        .context("listing user repositories")?;

    let all_pages = crab
        .all_pages(first_page)
        .await
        .context("paginating user repositories")?;

    let mut names: Vec<String> = all_pages.into_iter().filter_map(|r| r.full_name).collect();
    names.sort();
    Ok(names)
}

/// Lists every repo the token can reach — owned, collaborator and
/// organization-member, private included. Backs `add --all` and listing
/// your own account.
pub async fn list_authenticated_repos(crab: &Octocrab) -> Result<Vec<String>> {
    let first_page = crab
        .current()
        .list_repos_for_authenticated_user()
        .visibility("all")
        .affiliation("owner,collaborator,organization_member")
        .per_page(100)
        .send()
        .await
        .context("listing repositories for the authenticated user")?;

    let all_pages = crab
        .all_pages(first_page)
        .await
        .context("paginating authenticated repositories")?;

    let mut names: Vec<String> = all_pages.into_iter().filter_map(|r| r.full_name).collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Lists an org's repos, private included when the token is a member.
pub async fn list_org_repos(crab: &Octocrab, org: &str) -> Result<Vec<String>> {
    let first_page = crab
        .orgs(org)
        .list_repos()
        .repo_type(octocrab::params::repos::Type::All)
        .per_page(100)
        .send()
        .await
        .context("listing organization repositories")?;

    let all_pages = crab
        .all_pages(first_page)
        .await
        .context("paginating organization repositories")?;

    let mut names: Vec<String> = all_pages.into_iter().filter_map(|r| r.full_name).collect();
    names.sort();
    Ok(names)
}

/// The login the token authenticates as, or `None` when unauthenticated
/// (no token / 401) so callers can fall back to public listing.
pub async fn authenticated_login(crab: &Octocrab) -> Result<Option<String>> {
    match crab.current().user().await {
        Ok(user) => Ok(Some(user.login)),
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code.as_u16() == 401 => {
            Ok(None)
        }
        Err(e) => Err(e).context("identifying the authenticated user"),
    }
}

/// Whether `username` is a personal user or an organization.
pub async fn account_kind(crab: &Octocrab, username: &str) -> Result<AccountKind> {
    let profile = crab
        .users(username)
        .profile()
        .await
        .with_context(|| format!("fetching profile for {username}"))?;
    Ok(account_kind_from_type(&profile.r#type))
}

/// Resolves which `ListSource` to use for a concrete target, querying GitHub
/// for the authenticated login and the target's account kind as needed.
pub async fn resolve_source_for(crab: &Octocrab, username: &str) -> Result<ListSource> {
    let auth_login = authenticated_login(crab).await?;
    if let Some(login) = &auth_login
        && login.eq_ignore_ascii_case(username)
    {
        return Ok(ListSource::Authenticated);
    }
    let kind = account_kind(crab, username).await?;
    Ok(resolve_list_source(
        false,
        Some(username),
        auth_login.as_deref().unwrap_or(""),
        Some(kind),
    ))
}

#[derive(Debug, Deserialize)]
struct SubscribedIssue {
    number: u64,
    repository_url: String,
}

pub async fn fetch_subscribed_items(crab: &Octocrab) -> Result<SubscribedItems> {
    let first_page = crab
        .get::<octocrab::Page<SubscribedIssue>, _, _>(
            "/issues?filter=subscribed&state=open&per_page=100",
            None::<&()>,
        )
        .await
        .context("listing subscribed issues and pull requests (GITHUB_TOKEN is required)")?;
    let issues = crab
        .all_pages(first_page)
        .await
        .context("paginating subscribed issues and pull requests")?;

    Ok(index_subscribed_items(issues))
}

fn index_subscribed_items(issues: Vec<SubscribedIssue>) -> SubscribedItems {
    let mut subscribed = SubscribedItems::new();
    for issue in issues {
        let Some(repo) = repo_name_from_api_url(&issue.repository_url) else {
            continue;
        };
        subscribed.entry(repo).or_default().insert(issue.number);
    }
    subscribed
}

fn repo_name_from_api_url(url: &str) -> Option<String> {
    let (_, path) = url.split_once("/repos/")?;
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{name}").to_ascii_lowercase())
}

/// Fetches one repo's items over REST, consulting (and, when the repo is
/// unknown to it, extending) the fork cache. Returns the result plus, when a
/// repo's fork status was freshly determined this call, `(repo, value)` to
/// merge into the persisted cache (`value` is the upstream name, or `""` for
/// a confirmed non-fork).
pub async fn fetch_repo_items(
    crab: &Octocrab,
    repo: &str,
    subscribed_numbers: Option<&HashSet<u64>>,
    forks: &ForkCache,
) -> (RepoResult, Option<(String, String)>) {
    let Some((owner, name)) = split_repo(repo) else {
        return (RepoResult::new(repo.to_owned(), RepoStatus::NotFound), None);
    };

    match forks.get(repo) {
        Some(upstream) if !upstream.is_empty() => {
            let result = fetch_repo_items_upstream(crab, repo, owner, upstream).await;
            (result, None)
        }
        Some(_) => {
            let result = fetch_repo_items_normal(crab, repo, owner, name, subscribed_numbers).await;
            (result, None)
        }
        None => fetch_repo_items_detecting_fork(crab, repo, owner, name, subscribed_numbers).await,
    }
}

async fn fetch_repo_items_normal(
    crab: &Octocrab,
    repo: &str,
    owner: &str,
    name: &str,
    subscribed_numbers: Option<&HashSet<u64>>,
) -> RepoResult {
    match fetch_items_inner(crab, owner, name, subscribed_numbers).await {
        Ok(items) => RepoResult::new(repo.to_owned(), RepoStatus::Items(items)),
        Err(GithubError::NotFound(_)) => RepoResult::new(repo.to_owned(), RepoStatus::NotFound),
        Err(GithubError::Api(e)) => RepoResult::new(
            repo.to_owned(),
            RepoStatus::Error(RepoError::Api(describe_api_error(&e))),
        ),
    }
}

async fn fetch_repo_items_upstream(
    crab: &Octocrab,
    repo: &str,
    fork_owner: &str,
    upstream: &str,
) -> RepoResult {
    match fetch_upstream_items_rest(crab, fork_owner, upstream).await {
        Ok(items) => RepoResult::new(repo.to_owned(), RepoStatus::Items(items))
            .with_upstream(upstream.to_owned()),
        Err(GithubError::NotFound(_)) => RepoResult::new(repo.to_owned(), RepoStatus::NotFound)
            .with_upstream(upstream.to_owned()),
        Err(GithubError::Api(e)) => RepoResult::new(
            repo.to_owned(),
            RepoStatus::Error(RepoError::Api(describe_api_error(&e))),
        )
        .with_upstream(upstream.to_owned()),
    }
}

/// A repo not yet in the fork cache: one-time `GET /repos/{owner}/{name}` to
/// learn whether it's a fork, then fetch the right set of items. The
/// detection outcome is returned so the caller can persist it and skip this
/// GET on future runs.
async fn fetch_repo_items_detecting_fork(
    crab: &Octocrab,
    repo: &str,
    owner: &str,
    name: &str,
    subscribed_numbers: Option<&HashSet<u64>>,
) -> (RepoResult, Option<(String, String)>) {
    let repo_data = match crab.repos(owner, name).get().await {
        Ok(repo_data) => repo_data,
        Err(_) => {
            // Couldn't confirm fork status — fetch normally and leave the
            // cache untouched so we retry the check next run.
            let result = fetch_repo_items_normal(crab, repo, owner, name, subscribed_numbers).await;
            return (result, None);
        }
    };

    let upstream = if repo_data.fork.unwrap_or(false) {
        repo_data.parent.as_ref().and_then(|p| p.full_name.clone())
    } else {
        None
    };

    match upstream {
        Some(upstream) => {
            let result = fetch_repo_items_upstream(crab, repo, owner, &upstream).await;
            (result, Some((repo.to_owned(), upstream)))
        }
        None => {
            let result = fetch_repo_items_normal(crab, repo, owner, name, subscribed_numbers).await;
            (result, Some((repo.to_owned(), String::new())))
        }
    }
}

async fn fetch_upstream_items_rest(
    crab: &Octocrab,
    fork_owner: &str,
    upstream: &str,
) -> std::result::Result<Vec<RepoItem>, GithubError> {
    let Some((up_owner, up_name)) = split_repo(upstream) else {
        return Err(GithubError::NotFound(upstream.to_owned()));
    };
    let label = format!("{up_owner}/{up_name}");

    let first_page = crab
        .issues(up_owner, up_name)
        .list()
        .creator(fork_owner)
        .state(octocrab::params::State::Open)
        .per_page(100)
        .send()
        .await;
    let first_page = map_github_err(first_page, &label)?;
    let all_issues = crab.all_pages(first_page).await.map_err(GithubError::Api)?;

    let mut items: Vec<RepoItem> = Vec::with_capacity(all_issues.len());
    for issue in all_issues {
        // This endpoint returns issues and PRs together; PRs carry a
        // `pull_request` link.
        let kind = if issue.pull_request.is_some() {
            ItemKind::PullRequest
        } else {
            ItemKind::Issue
        };
        items.push(RepoItem {
            kind,
            number: issue.number,
            title: issue.title,
            created_at: issue.created_at,
            updated_at: issue.updated_at,
            author: issue.user.login,
            pr_draft: None,
            comments: Some(u64::from(issue.comments)),
            review_decision: None,
        });
    }
    items.sort_by(item_cmp);
    Ok(items)
}

/// Fetches every repo over REST, capped at `MAX_CONCURRENT_FETCHES`
/// in-flight requests and an overall `FETCH_TIMEOUT` deadline for the whole
/// batch (repos still in flight when the deadline hits are reported as
/// timeouts). Used when there's no `GITHUB_TOKEN` — GraphQL has no
/// anonymous mode — and as the overflow fallback for repos whose open
/// issues/PRs exceed a single GraphQL page.
pub(crate) async fn fetch_repos_rest(
    crab: &Octocrab,
    repos: &[String],
    subscribed: Option<&SubscribedItems>,
    forks: &ForkCache,
) -> (Vec<RepoResult>, ForkCache) {
    let empty_subscriptions = HashSet::new();
    let mut results = vec![None; repos.len()];
    let mut detected: ForkCache = ForkCache::new();
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
            forks,
        ));
        next += 1;
    }

    let deadline = time::sleep(FETCH_TIMEOUT);
    tokio::pin!(deadline);

    while !in_flight.is_empty() {
        tokio::select! {
            _ = &mut deadline => break,
            Some((index, result, detected_fork)) = in_flight.next() => {
                results[index] = Some(result);
                if let Some((key, value)) = detected_fork {
                    detected.insert(key, value);
                }

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
                        forks,
                    ));
                    next += 1;
                }
            }
        }
    }

    let results = results
        .into_iter()
        .enumerate()
        .map(|(index, result)| result.unwrap_or_else(|| timeout_result(repos[index].clone())))
        .collect();
    (results, detected)
}

async fn fetch_repo_with_timeout(
    crab: &Octocrab,
    index: usize,
    repo: String,
    subscribed_numbers: Option<&HashSet<u64>>,
    forks: &ForkCache,
) -> (usize, RepoResult, Option<(String, String)>) {
    let (result, detected) = match timeout(
        FETCH_TIMEOUT,
        fetch_repo_items(crab, &repo, subscribed_numbers, forks),
    )
    .await
    {
        Ok(pair) => pair,
        Err(_) => (timeout_result(repo), None),
    };
    (index, result, detected)
}

fn timeout_result(repo: String) -> RepoResult {
    RepoResult::new(repo, RepoStatus::Error(RepoError::Timeout))
}

async fn fetch_items_inner(
    crab: &Octocrab,
    owner: &str,
    name: &str,
    subscribed_numbers: Option<&HashSet<u64>>,
) -> std::result::Result<Vec<RepoItem>, GithubError> {
    let label = format!("{owner}/{name}");

    let issues_handler = crab.issues(owner, name);
    let issues_future = issues_handler
        .list()
        .state(octocrab::params::State::Open)
        .per_page(100)
        .send();

    let prs_handler = crab.pulls(owner, name);
    let prs_future = prs_handler
        .list()
        .state(octocrab::params::State::Open)
        .per_page(100)
        .send();

    let (issues_res, prs_res) = futures::future::join(issues_future, prs_future).await;

    let issues_page = map_github_err(issues_res, &label)?;
    let prs_page = map_github_err(prs_res, &label)?;

    let all_issues = crab
        .all_pages(issues_page)
        .await
        .map_err(GithubError::Api)?;
    let all_prs = crab.all_pages(prs_page).await.map_err(GithubError::Api)?;

    let mut items: Vec<RepoItem> = Vec::new();

    for issue in all_issues {
        // Skip PRs that appear in the issues endpoint
        if issue.pull_request.is_some() {
            continue;
        }
        let author = issue.user.login.clone();
        let created_at = issue.created_at;
        let updated_at = issue.updated_at;
        items.push(RepoItem {
            kind: ItemKind::Issue,
            number: issue.number,
            title: issue.title,
            created_at,
            updated_at,
            author,
            pr_draft: None,
            comments: None,
            review_decision: None,
        });
    }

    for pr in all_prs {
        // octocrab 0.54 models most PR-list fields as Option; a PR returned
        // by the list endpoint always carries them in practice, so fall back
        // rather than fail the whole repo.
        let author = pr
            .user
            .as_ref()
            .map_or_else(|| "ghost".to_owned(), |user| user.login.clone());
        let created_at = pr.created_at.or(pr.updated_at).unwrap_or_else(Utc::now);
        let updated_at = pr.updated_at.unwrap_or(created_at);
        let pr_draft = pr.draft;
        items.push(RepoItem {
            kind: ItemKind::PullRequest,
            number: pr.number,
            title: pr.title.unwrap_or_default(),
            created_at,
            updated_at,
            author,
            pr_draft,
            comments: None,
            review_decision: None,
        });
    }

    retain_subscribed(&mut items, subscribed_numbers);

    // Sort: PRs first, then issues; within each group by number descending
    items.sort_by(item_cmp);

    Ok(items)
}

pub(crate) fn retain_subscribed(
    items: &mut Vec<RepoItem>,
    subscribed_numbers: Option<&HashSet<u64>>,
) {
    if let Some(numbers) = subscribed_numbers {
        items.retain(|item| numbers.contains(&item.number));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_item(kind: ItemKind, number: u64) -> RepoItem {
        RepoItem {
            kind,
            number,
            title: format!("item {number}"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            author: "user".into(),
            pr_draft: None,
            comments: None,
            review_decision: None,
        }
    }

    #[test]
    fn account_kind_organization() {
        assert_eq!(
            account_kind_from_type("Organization"),
            AccountKind::Organization
        );
    }

    #[test]
    fn account_kind_user() {
        assert_eq!(account_kind_from_type("User"), AccountKind::User);
    }

    #[test]
    fn account_kind_unknown_defaults_to_user() {
        assert_eq!(account_kind_from_type("Bot"), AccountKind::User);
    }

    #[test]
    fn all_flag_lists_authenticated() {
        assert_eq!(
            resolve_list_source(true, None, "me", None),
            ListSource::Authenticated
        );
    }

    #[test]
    fn own_username_lists_authenticated() {
        assert_eq!(
            resolve_list_source(false, Some("me"), "me", Some(AccountKind::User)),
            ListSource::Authenticated
        );
    }

    #[test]
    fn own_username_is_case_insensitive() {
        assert_eq!(
            resolve_list_source(false, Some("ME"), "me", Some(AccountKind::User)),
            ListSource::Authenticated
        );
    }

    #[test]
    fn org_target_lists_org_repos() {
        assert_eq!(
            resolve_list_source(false, Some("acme"), "me", Some(AccountKind::Organization)),
            ListSource::Org("acme".to_owned())
        );
    }

    #[test]
    fn third_party_user_lists_public_only() {
        assert_eq!(
            resolve_list_source(false, Some("octocat"), "me", Some(AccountKind::User)),
            ListSource::PublicUser("octocat".to_owned())
        );
    }

    #[test]
    fn split_repo_valid() {
        assert_eq!(split_repo("a/b"), Some(("a", "b")));
    }

    #[test]
    fn split_repo_no_slash() {
        assert_eq!(split_repo("abc"), None);
    }

    #[test]
    fn split_repo_trailing_slash() {
        assert_eq!(split_repo("a/"), None);
    }

    #[test]
    fn split_repo_leading_slash() {
        assert_eq!(split_repo("/b"), None);
    }

    #[test]
    fn split_repo_many_slashes() {
        // splitn(2) gives ("a", "b/c") — name contains a slash, which is fine
        assert_eq!(split_repo("a/b/c"), Some(("a", "b/c")));
    }

    #[test]
    fn subscribed_index_and_filter_keep_only_matching_repo_items() {
        let subscribed = index_subscribed_items(vec![
            SubscribedIssue {
                number: 7,
                repository_url: "https://api.github.com/repos/Acme/Widget".into(),
            },
            SubscribedIssue {
                number: 99,
                repository_url: "https://api.github.com/users/octocat".into(),
            },
        ]);
        let mut items = vec![
            make_item(ItemKind::Issue, 7),
            make_item(ItemKind::PullRequest, 9),
        ];

        retain_subscribed(&mut items, subscribed.get("acme/widget"));

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].number, 7);
    }

    #[test]
    fn missing_subscription_filter_keeps_all_repo_items() {
        let mut items = vec![
            make_item(ItemKind::Issue, 7),
            make_item(ItemKind::PullRequest, 9),
        ];

        retain_subscribed(&mut items, None);

        assert_eq!(items.len(), 2);
    }

    #[test]
    fn item_cmp_sorts_prs_before_issues_then_number_desc() {
        let mut items = [
            make_item(ItemKind::Issue, 5),
            make_item(ItemKind::PullRequest, 2),
            make_item(ItemKind::Issue, 10),
            make_item(ItemKind::PullRequest, 8),
        ];
        items.sort_by(item_cmp);
        let numbers: Vec<u64> = items.iter().map(|i| i.number).collect();
        assert_eq!(numbers, vec![8, 2, 10, 5]);
    }
}
