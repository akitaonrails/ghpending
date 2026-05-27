use std::cmp::Ordering;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use octocrab::Octocrab;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct RepoItem {
    pub kind: ItemKind,
    pub number: u64,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub author: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ItemKind {
    PullRequest,
    Issue,
}

#[derive(Debug, Clone)]
pub struct RepoResult {
    pub repo: String,
    pub status: RepoStatus,
}

#[derive(Debug, Clone)]
pub enum RepoStatus {
    Items(Vec<RepoItem>),
    NotFound,
    Error(String),
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

pub async fn fetch_repo_items(crab: &Octocrab, repo: &str) -> RepoResult {
    let Some((owner, name)) = split_repo(repo) else {
        return RepoResult {
            repo: repo.to_owned(),
            status: RepoStatus::NotFound,
        };
    };

    match fetch_items_inner(crab, owner, name).await {
        Ok(items) => RepoResult {
            repo: repo.to_owned(),
            status: RepoStatus::Items(items),
        },
        Err(GithubError::NotFound(_)) => RepoResult {
            repo: repo.to_owned(),
            status: RepoStatus::NotFound,
        },
        Err(GithubError::Api(e)) => RepoResult {
            repo: repo.to_owned(),
            status: RepoStatus::Error(e.to_string()),
        },
    }
}

async fn fetch_items_inner(
    crab: &Octocrab,
    owner: &str,
    name: &str,
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
        items.push(RepoItem {
            kind: ItemKind::Issue,
            number: issue.number,
            title: issue.title,
            created_at,
            author,
        });
    }

    for pr in all_prs {
        let author = pr.user.login.clone();
        let created_at = pr.created_at;
        items.push(RepoItem {
            kind: ItemKind::PullRequest,
            number: pr.number,
            title: pr.title,
            created_at,
            author,
        });
    }

    // Sort: PRs first, then issues; within each group by number descending
    items.sort_by(item_cmp);

    Ok(items)
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
            author: "user".into(),
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
