//! GitLab API v4 → the provider-neutral model in [`crate::model`].
//!
//! Field mapping: `iid` → `number`, `author.username` → `author`, and a merge
//! request's `draft` → `pr_draft`. GitLab's issues endpoint never returns merge
//! requests, so unlike GitHub no filtering is needed.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;

use crate::gitlab_client::{GitlabClient, GitlabHttpError};
use crate::model::{ItemKind, RepoError, RepoItem, RepoResult, RepoStatus, item_cmp};

const PER_PAGE: u32 = 100;
/// Guards against a server that keeps handing out `x-next-page` forever.
const MAX_PAGES: u32 = 20;

const OPEN_ITEMS_FILTER: &str = "state=opened";
/// `simple=true` trims the payload to the handful of fields we read.
const MEMBERSHIP_PROJECTS_FILTER: &str = "membership=true&simple=true";
const GROUP_PROJECTS_FILTER: &str = "include_subgroups=true&simple=true";

#[derive(Debug, Deserialize)]
struct GitlabAuthor {
    username: String,
}

#[derive(Debug, Deserialize)]
struct GitlabIssue {
    iid: u64,
    title: String,
    created_at: DateTime<Utc>,
    author: Option<GitlabAuthor>,
}

#[derive(Debug, Deserialize)]
struct GitlabMr {
    iid: u64,
    title: String,
    created_at: DateTime<Utc>,
    author: Option<GitlabAuthor>,
    #[serde(default)]
    draft: bool,
}

/// Percent-encodes a project path for use as a path segment — GitLab addresses
/// projects as `group/subgroup/app`, which must arrive as `group%2Fsubgroup%2Fapp`.
fn encode_project_path(path: &str) -> String {
    utf8_percent_encode(path.trim(), NON_ALPHANUMERIC).to_string()
}

/// Fetches every open issue and merge request of one project as a `RepoResult`.
/// `label` is what the digest shows (e.g. `gitlab.com/group/app`).
pub async fn fetch_project_items(
    client: &GitlabClient,
    project_path: &str,
    label: &str,
) -> RepoResult {
    let status = match fetch_items_inner(client, project_path).await {
        Ok(items) => RepoStatus::Items(items),
        Err(GitlabHttpError::Status(code)) if code.as_u16() == 404 => RepoStatus::NotFound,
        Err(GitlabHttpError::Status(code)) => {
            RepoStatus::Error(RepoError::Api(format!("http {code}")))
        }
        Err(GitlabHttpError::Other(e)) => RepoStatus::Error(RepoError::Api(e.to_string())),
    };

    RepoResult {
        repo: label.to_owned(),
        status,
    }
}

async fn fetch_items_inner(
    client: &GitlabClient,
    project_path: &str,
) -> std::result::Result<Vec<RepoItem>, GitlabHttpError> {
    let encoded = encode_project_path(project_path);
    let issues_path = format!("/projects/{encoded}/issues");
    let mrs_path = format!("/projects/{encoded}/merge_requests");

    let (issues, mrs) = futures::future::join(
        fetch_all_pages::<GitlabIssue>(client, &issues_path, OPEN_ITEMS_FILTER),
        fetch_all_pages::<GitlabMr>(client, &mrs_path, OPEN_ITEMS_FILTER),
    )
    .await;

    let mut items: Vec<RepoItem> = Vec::new();

    for mr in mrs? {
        items.push(RepoItem {
            kind: ItemKind::PullRequest,
            number: mr.iid,
            title: mr.title,
            created_at: mr.created_at,
            author: author_name(mr.author),
            pr_draft: Some(mr.draft),
        });
    }

    for issue in issues? {
        items.push(RepoItem {
            kind: ItemKind::Issue,
            number: issue.iid,
            title: issue.title,
            created_at: issue.created_at,
            author: author_name(issue.author),
            pr_draft: None,
        });
    }

    items.sort_by(item_cmp);

    Ok(items)
}

/// GitLab omits `author` for items created by a since-deleted account.
fn author_name(author: Option<GitlabAuthor>) -> String {
    author
        .map(|a| a.username)
        .unwrap_or_else(|| "unknown".into())
}

/// Walks `x-next-page` until the server stops sending one. `filters` is the
/// endpoint-specific part of the query; paging is appended here.
async fn fetch_all_pages<T: serde::de::DeserializeOwned>(
    client: &GitlabClient,
    path: &str,
    filters: &str,
) -> std::result::Result<Vec<T>, GitlabHttpError> {
    let mut all: Vec<T> = Vec::new();
    let mut page: Option<String> = None;

    for _ in 0..MAX_PAGES {
        let query = build_query(filters, page.as_deref());
        let response = client.get_json::<Vec<T>>(path, &query).await?;
        all.extend(response.items);

        match response.next_page {
            Some(next) => page = Some(next),
            None => break,
        }
    }

    Ok(all)
}

fn build_query(filters: &str, page: Option<&str>) -> String {
    let base = format!("{filters}&per_page={PER_PAGE}");
    match page {
        Some(page) => format!("{base}&page={page}"),
        None => base,
    }
}

/// Digest label for a GitLab project: `{host}/{path}`, so a GitLab entry is
/// visually distinguishable from a GitHub `owner/repo` in the same digest.
pub fn project_label(host: &str, project_path: &str) -> String {
    format!("{host}/{}", project_path.trim().trim_matches('/'))
}

#[derive(Debug, Deserialize)]
struct GitlabProject {
    path_with_namespace: String,
}

/// Every project the token is a member of. Requires authentication — an
/// anonymous call has no "membership" to speak of.
pub async fn list_membership_projects(client: &GitlabClient) -> Result<Vec<String>> {
    let projects: Vec<GitlabProject> =
        fetch_all_pages(client, "/projects", MEMBERSHIP_PROJECTS_FILTER)
            .await
            .map_err(describe_listing_error)
            .context("listing GitLab projects")?;
    Ok(sorted_paths(projects))
}

/// Projects of one group, subgroups included.
pub async fn list_group_projects(client: &GitlabClient, group: &str) -> Result<Vec<String>> {
    let path = format!("/groups/{}/projects", encode_project_path(group));
    let projects: Vec<GitlabProject> = fetch_all_pages(client, &path, GROUP_PROJECTS_FILTER)
        .await
        .map_err(describe_listing_error)
        .with_context(|| format!("listing projects of GitLab group {group}"))?;
    Ok(sorted_paths(projects))
}

fn sorted_paths(projects: Vec<GitlabProject>) -> Vec<String> {
    let mut paths: Vec<String> = projects
        .into_iter()
        .map(|p| p.path_with_namespace)
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Turns the bare HTTP status of a failed listing into something actionable —
/// 401/403 almost always means a missing or under-scoped `GITLAB_TOKEN`.
fn describe_listing_error(err: GitlabHttpError) -> anyhow::Error {
    match err {
        GitlabHttpError::Status(code) if code.as_u16() == 401 || code.as_u16() == 403 => {
            anyhow::anyhow!(
                "GitLab refused the request (http {code}). Set GITLAB_TOKEN to a \
                 personal access token with the `read_api` scope."
            )
        }
        GitlabHttpError::Status(code) if code.as_u16() == 404 => {
            anyhow::anyhow!("not found (http 404) — check the group path and your access")
        }
        GitlabHttpError::Status(code) => anyhow::anyhow!("http {code}"),
        GitlabHttpError::Other(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_slashes_in_nested_project_path() {
        assert_eq!(
            encode_project_path("grupo/subgrupo/app"),
            "grupo%2Fsubgrupo%2Fapp"
        );
    }

    #[test]
    fn encodes_simple_project_path() {
        assert_eq!(
            encode_project_path("gitlab-org/gitlab-runner"),
            "gitlab%2Dorg%2Fgitlab%2Drunner"
        );
    }

    #[test]
    fn encode_trims_surrounding_whitespace() {
        assert_eq!(encode_project_path("  a/b  "), "a%2Fb");
    }

    #[test]
    fn query_omits_page_on_first_request() {
        assert_eq!(
            build_query(OPEN_ITEMS_FILTER, None),
            "state=opened&per_page=100"
        );
    }

    #[test]
    fn query_carries_next_page_cursor() {
        assert_eq!(
            build_query(OPEN_ITEMS_FILTER, Some("3")),
            "state=opened&per_page=100&page=3"
        );
    }

    #[test]
    fn listing_queries_carry_their_own_filters() {
        assert_eq!(
            build_query(MEMBERSHIP_PROJECTS_FILTER, None),
            "membership=true&simple=true&per_page=100"
        );
        assert_eq!(
            build_query(GROUP_PROJECTS_FILTER, Some("2")),
            "include_subgroups=true&simple=true&per_page=100&page=2"
        );
    }

    #[test]
    fn project_listing_is_sorted_and_deduped() {
        let projects = vec![
            GitlabProject {
                path_with_namespace: "b/two".into(),
            },
            GitlabProject {
                path_with_namespace: "a/one".into(),
            },
            GitlabProject {
                path_with_namespace: "b/two".into(),
            },
        ];
        assert_eq!(sorted_paths(projects), vec!["a/one", "b/two"]);
    }

    #[test]
    fn nested_group_path_is_encoded_for_the_groups_endpoint() {
        // A subgroup path must survive as one path segment.
        assert_eq!(
            encode_project_path("defensoria/solar"),
            "defensoria%2Fsolar"
        );
    }

    #[test]
    fn unauthorized_listing_error_points_at_the_token() {
        let msg = describe_listing_error(GitlabHttpError::Status(http::StatusCode::UNAUTHORIZED))
            .to_string();
        assert!(msg.contains("GITLAB_TOKEN"), "unexpected message: {msg}");

        let forbidden =
            describe_listing_error(GitlabHttpError::Status(http::StatusCode::FORBIDDEN))
                .to_string();
        assert!(forbidden.contains("GITLAB_TOKEN"));
    }

    #[test]
    fn not_found_listing_error_mentions_the_group_path() {
        let msg = describe_listing_error(GitlabHttpError::Status(http::StatusCode::NOT_FOUND))
            .to_string();
        assert!(msg.contains("group path"), "unexpected message: {msg}");
    }

    #[test]
    fn deserializes_project_listing_payload() {
        let json = r#"[{"id":1,"path_with_namespace":"defensoria/solar/solar-backend",
                        "name":"solar-backend","web_url":"https://x/y"}]"#;
        let projects: Vec<GitlabProject> = serde_json::from_str(json).unwrap();
        assert_eq!(
            sorted_paths(projects),
            vec!["defensoria/solar/solar-backend"]
        );
    }

    #[test]
    fn label_prefixes_project_with_host() {
        assert_eq!(
            project_label("gitlab.com", "gitlab-org/gitlab-runner"),
            "gitlab.com/gitlab-org/gitlab-runner"
        );
        assert_eq!(
            project_label("gitlab.dpe.br", "/nucleo-ti/portal/"),
            "gitlab.dpe.br/nucleo-ti/portal"
        );
    }

    #[test]
    fn deserializes_issue_json_into_the_neutral_model() {
        let json = r#"[{
            "iid": 42,
            "title": "Login quebra com SSO",
            "created_at": "2024-03-11T14:05:00.000Z",
            "author": { "username": "aluna" }
        }]"#;
        let issues: Vec<GitlabIssue> = serde_json::from_str(json).unwrap();
        let issue = issues.into_iter().next().unwrap();

        assert_eq!(issue.iid, 42);
        assert_eq!(issue.title, "Login quebra com SSO");
        assert_eq!(author_name(issue.author), "aluna");
        assert_eq!(issue.created_at.to_rfc3339(), "2024-03-11T14:05:00+00:00");
    }

    #[test]
    fn deserializes_merge_request_draft_flag() {
        let json = r#"[
            { "iid": 7, "title": "WIP", "created_at": "2024-03-11T14:05:00.000Z",
              "author": { "username": "dev" }, "draft": true },
            { "iid": 8, "title": "Ready", "created_at": "2024-03-11T14:05:00.000Z",
              "author": { "username": "dev" } }
        ]"#;
        let mrs: Vec<GitlabMr> = serde_json::from_str(json).unwrap();
        assert!(mrs[0].draft);
        // `draft` absent → not a draft, rather than a parse failure.
        assert!(!mrs[1].draft);
    }

    #[test]
    fn missing_author_falls_back_to_unknown() {
        let json = r#"[{
            "iid": 1, "title": "Órfã", "created_at": "2024-03-11T14:05:00.000Z",
            "author": null
        }]"#;
        let issues: Vec<GitlabIssue> = serde_json::from_str(json).unwrap();
        assert_eq!(
            author_name(issues.into_iter().next().unwrap().author),
            "unknown"
        );
    }

    #[test]
    fn ignores_unknown_api_fields() {
        // GitLab returns dozens of fields we do not model; they must not break parsing.
        let json = r#"[{
            "id": 999, "iid": 3, "project_id": 12, "title": "t",
            "created_at": "2024-03-11T14:05:00.000Z", "state": "opened",
            "author": { "id": 5, "username": "u", "name": "U" },
            "labels": ["bug"], "web_url": "https://gitlab.com/x/-/issues/3"
        }]"#;
        let issues: Vec<GitlabIssue> = serde_json::from_str(json).unwrap();
        assert_eq!(issues[0].iid, 3);
    }
}
