//! GitLab API v4 → the provider-neutral model in [`crate::model`].
//!
//! Field mapping: `iid` → `number`, `author.username` → `author`, and a merge
//! request's `draft` → `pr_draft`. GitLab's issues endpoint never returns merge
//! requests, so unlike GitHub no filtering is needed.

use chrono::{DateTime, Utc};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;

use crate::gitlab_client::{GitlabClient, GitlabHttpError};
use crate::model::{ItemKind, RepoError, RepoItem, RepoResult, RepoStatus, item_cmp};

const PER_PAGE: u32 = 100;
/// Guards against a server that keeps handing out `x-next-page` forever.
const MAX_PAGES: u32 = 20;

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
        fetch_all_pages::<GitlabIssue>(client, &issues_path),
        fetch_all_pages::<GitlabMr>(client, &mrs_path),
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

/// Walks `x-next-page` until the server stops sending one.
async fn fetch_all_pages<T: serde::de::DeserializeOwned>(
    client: &GitlabClient,
    path: &str,
) -> std::result::Result<Vec<T>, GitlabHttpError> {
    let mut all: Vec<T> = Vec::new();
    let mut page: Option<String> = None;

    for _ in 0..MAX_PAGES {
        let query = build_query(page.as_deref());
        let response = client.get_json::<Vec<T>>(path, &query).await?;
        all.extend(response.items);

        match response.next_page {
            Some(next) => page = Some(next),
            None => break,
        }
    }

    Ok(all)
}

fn build_query(page: Option<&str>) -> String {
    let base = format!("state=opened&per_page={PER_PAGE}");
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
        assert_eq!(build_query(None), "state=opened&per_page=100");
    }

    #[test]
    fn query_carries_next_page_cursor() {
        assert_eq!(build_query(Some("3")), "state=opened&per_page=100&page=3");
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
