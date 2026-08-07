use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::stream::{FuturesUnordered, StreamExt};
use octocrab::Octocrab;
use serde::Deserialize;
use tokio::time::timeout;

use crate::github::{
    self, ItemKind, RepoError, RepoItem, RepoResult, RepoStatus, SubscribedItems, item_cmp,
    retain_subscribed, split_repo,
};

const CHUNK_SIZE: usize = 40;
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONCURRENT_CHUNKS: usize = 4;
const MAX_ITEMS_PER_CONNECTION: u64 = 100;

#[derive(Debug, Deserialize, Default)]
struct GraphQlEnvelope {
    data: Option<HashMap<String, Option<RepoNode>>>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    #[serde(rename = "type")]
    error_type: Option<String>,
    path: Option<Vec<serde_json::Value>>,
    message: String,
}

#[derive(Debug, Deserialize)]
struct RepoNode {
    issues: Connection,
    #[serde(rename = "pullRequests")]
    pull_requests: Connection,
}

#[derive(Debug, Deserialize)]
struct Connection {
    #[serde(rename = "totalCount")]
    total_count: u64,
    nodes: Vec<ItemNode>,
}

#[derive(Debug, Deserialize)]
struct ItemNode {
    number: u64,
    title: String,
    #[serde(rename = "createdAt")]
    created_at: DateTime<Utc>,
    #[serde(rename = "updatedAt")]
    updated_at: DateTime<Utc>,
    author: Option<Author>,
    #[serde(default, rename = "isDraft")]
    is_draft: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct Author {
    login: String,
}

struct ValidRepo<'a> {
    original_index: usize,
    repo: &'a str,
    owner: &'a str,
    name: &'a str,
}

/// One repo's outcome from a batch, plus whether it overflowed the 100-item
/// page (needs a follow-up exhaustive REST fetch for full parity).
struct ChunkItem {
    original_index: usize,
    repo: String,
    status: RepoStatus,
    needs_fallback: bool,
}

pub async fn fetch_repos_batched(
    crab: &Octocrab,
    repos: &[String],
    subscribed: Option<&SubscribedItems>,
) -> Vec<RepoResult> {
    let mut results: Vec<Option<RepoResult>> = vec![None; repos.len()];
    let mut valid: Vec<ValidRepo> = Vec::new();

    for (index, repo) in repos.iter().enumerate() {
        match split_repo(repo) {
            Some((owner, name)) => valid.push(ValidRepo {
                original_index: index,
                repo,
                owner,
                name,
            }),
            None => {
                results[index] = Some(RepoResult {
                    repo: repo.clone(),
                    status: RepoStatus::NotFound,
                });
            }
        }
    }

    let chunks: Vec<&[ValidRepo]> = valid.chunks(CHUNK_SIZE).collect();
    let mut overflow: Vec<(usize, String)> = Vec::new();

    let mut in_flight = FuturesUnordered::new();
    let mut next_chunk = 0;

    while next_chunk < chunks.len() && in_flight.len() < MAX_CONCURRENT_CHUNKS {
        in_flight.push(fetch_chunk(crab, chunks[next_chunk], subscribed));
        next_chunk += 1;
    }

    while let Some(chunk_items) = in_flight.next().await {
        for item in chunk_items {
            if item.needs_fallback {
                overflow.push((item.original_index, item.repo.clone()));
            }
            results[item.original_index] = Some(RepoResult {
                repo: item.repo,
                status: item.status,
            });
        }
        if next_chunk < chunks.len() {
            in_flight.push(fetch_chunk(crab, chunks[next_chunk], subscribed));
            next_chunk += 1;
        }
    }

    for (original_index, repo) in overflow {
        let empty = std::collections::HashSet::new();
        let key = repo.to_ascii_lowercase();
        let subscribed_numbers = subscribed.map(|items| items.get(&key).unwrap_or(&empty));
        let result = github::fetch_repo_items(crab, &repo, subscribed_numbers).await;
        results[original_index] = Some(result);
    }

    results
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            result.unwrap_or_else(|| RepoResult {
                repo: repos[index].clone(),
                status: RepoStatus::Error(RepoError::Timeout),
            })
        })
        .collect()
}

async fn fetch_chunk(
    crab: &Octocrab,
    chunk: &[ValidRepo<'_>],
    subscribed: Option<&SubscribedItems>,
) -> Vec<ChunkItem> {
    let query = build_query(chunk);
    let body = serde_json::json!({ "query": query });

    let response = match timeout(
        CHUNK_TIMEOUT,
        crab.post::<_, GraphQlEnvelope>("/graphql", Some(&body)),
    )
    .await
    {
        Ok(Ok(envelope)) => envelope,
        Ok(Err(e)) => return error_for_all(chunk, RepoError::Api(e.to_string())),
        Err(_) => return error_for_all(chunk, RepoError::Timeout),
    };

    parse_envelope(chunk, response, subscribed)
}

fn error_for_all(chunk: &[ValidRepo<'_>], error: RepoError) -> Vec<ChunkItem> {
    chunk
        .iter()
        .map(|repo| ChunkItem {
            original_index: repo.original_index,
            repo: repo.repo.to_owned(),
            status: RepoStatus::Error(error.clone()),
            needs_fallback: false,
        })
        .collect()
}

fn build_query(chunk: &[ValidRepo<'_>]) -> String {
    let mut query = String::from("query {");
    for (i, repo) in chunk.iter().enumerate() {
        query.push_str(&format!(
            "\n  r{i}: repository(owner: {:?}, name: {:?}) {{\n    issues(states: OPEN, first: {MAX_ITEMS_PER_CONNECTION}) {{ totalCount nodes {{ number title createdAt updatedAt author {{ login }} }} }}\n    pullRequests(states: OPEN, first: {MAX_ITEMS_PER_CONNECTION}) {{ totalCount nodes {{ number title createdAt updatedAt author {{ login }} isDraft }} }}\n  }}",
            repo.owner, repo.name,
        ));
    }
    query.push_str("\n}");
    query
}

fn parse_envelope(
    chunk: &[ValidRepo<'_>],
    envelope: GraphQlEnvelope,
    subscribed: Option<&SubscribedItems>,
) -> Vec<ChunkItem> {
    let mut not_found: HashMap<String, ()> = HashMap::new();
    let mut errored: HashMap<String, String> = HashMap::new();

    for error in envelope.errors.into_iter().flatten() {
        let Some(alias) = error
            .path
            .as_ref()
            .and_then(|p| p.first())
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        let alias = alias.to_owned();
        if error.error_type.as_deref() == Some("NOT_FOUND") {
            not_found.insert(alias, ());
        } else {
            errored.entry(alias).or_insert(error.message.clone());
        }
    }

    let data = envelope.data.unwrap_or_default();
    let empty = std::collections::HashSet::new();

    chunk
        .iter()
        .enumerate()
        .map(|(i, repo)| {
            let alias = format!("r{i}");
            let subscribed_numbers = subscribed
                .map(|items| items.get(&repo.repo.to_ascii_lowercase()).unwrap_or(&empty));

            if not_found.contains_key(alias.as_str()) {
                return ChunkItem {
                    original_index: repo.original_index,
                    repo: repo.repo.to_owned(),
                    status: RepoStatus::NotFound,
                    needs_fallback: false,
                };
            }
            if let Some(message) = errored.get(alias.as_str()) {
                return ChunkItem {
                    original_index: repo.original_index,
                    repo: repo.repo.to_owned(),
                    status: RepoStatus::Error(RepoError::Api(message.clone())),
                    needs_fallback: false,
                };
            }

            match data.get(&alias) {
                Some(Some(node)) => {
                    let needs_fallback = node.issues.total_count > MAX_ITEMS_PER_CONNECTION
                        || node.pull_requests.total_count > MAX_ITEMS_PER_CONNECTION;
                    let mut items = items_from_node(node);
                    retain_subscribed(&mut items, subscribed_numbers);
                    items.sort_by(item_cmp);
                    ChunkItem {
                        original_index: repo.original_index,
                        repo: repo.repo.to_owned(),
                        status: RepoStatus::Items(items),
                        needs_fallback,
                    }
                }
                _ => ChunkItem {
                    original_index: repo.original_index,
                    repo: repo.repo.to_owned(),
                    status: RepoStatus::NotFound,
                    needs_fallback: false,
                },
            }
        })
        .collect()
}

fn items_from_node(node: &RepoNode) -> Vec<RepoItem> {
    let mut items = Vec::with_capacity(node.issues.nodes.len() + node.pull_requests.nodes.len());

    for issue in &node.issues.nodes {
        items.push(RepoItem {
            kind: ItemKind::Issue,
            number: issue.number,
            title: issue.title.clone(),
            created_at: issue.created_at,
            updated_at: issue.updated_at,
            author: author_login(&issue.author),
            pr_draft: None,
        });
    }
    for pr in &node.pull_requests.nodes {
        items.push(RepoItem {
            kind: ItemKind::PullRequest,
            number: pr.number,
            title: pr.title.clone(),
            created_at: pr.created_at,
            updated_at: pr.updated_at,
            author: author_login(&pr.author),
            pr_draft: pr.is_draft,
        });
    }

    items
}

fn author_login(author: &Option<Author>) -> String {
    author
        .as_ref()
        .map(|a| a.login.clone())
        .unwrap_or_else(|| "ghost".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_repo(original_index: usize, repo: &str) -> ValidRepo<'_> {
        let (owner, name) = split_repo(repo).unwrap();
        ValidRepo {
            original_index,
            repo,
            owner,
            name,
        }
    }

    #[test]
    fn build_query_aliases_repos_in_order() {
        let chunk = vec![valid_repo(0, "acme/widget"), valid_repo(1, "acme/gadget")];
        let query = build_query(&chunk);
        assert!(query.contains(r#"r0: repository(owner: "acme", name: "widget")"#));
        assert!(query.contains(r#"r1: repository(owner: "acme", name: "gadget")"#));
        assert!(query.contains("states: OPEN"));
    }

    #[test]
    fn parses_normal_repo_with_items() {
        let chunk = vec![valid_repo(5, "acme/widget")];
        let envelope: GraphQlEnvelope = serde_json::from_value(serde_json::json!({
            "data": {
                "r0": {
                    "issues": { "totalCount": 1, "nodes": [
                        { "number": 3, "title": "bug", "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z", "author": { "login": "alice" } }
                    ] },
                    "pullRequests": { "totalCount": 1, "nodes": [
                        { "number": 7, "title": "feature", "createdAt": "2026-01-02T00:00:00Z", "updatedAt": "2026-01-02T00:00:00Z", "author": { "login": "bob" }, "isDraft": true }
                    ] }
                }
            }
        }))
        .unwrap();

        let result = parse_envelope(&chunk, envelope, None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].original_index, 5);
        assert!(!result[0].needs_fallback);
        let RepoStatus::Items(items) = &result[0].status else {
            panic!("expected items")
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, ItemKind::PullRequest);
        assert_eq!(items[0].number, 7);
        assert_eq!(items[1].kind, ItemKind::Issue);
    }

    #[test]
    fn not_found_error_maps_to_not_found_status_without_poisoning_other_aliases() {
        let chunk = vec![valid_repo(0, "acme/gone"), valid_repo(1, "acme/widget")];
        let envelope: GraphQlEnvelope = serde_json::from_value(serde_json::json!({
            "data": {
                "r0": null,
                "r1": {
                    "issues": { "totalCount": 0, "nodes": [] },
                    "pullRequests": { "totalCount": 0, "nodes": [] }
                }
            },
            "errors": [
                { "type": "NOT_FOUND", "path": ["r0"], "message": "Could not resolve to a Repository" }
            ]
        }))
        .unwrap();

        let result = parse_envelope(&chunk, envelope, None);
        assert!(matches!(result[0].status, RepoStatus::NotFound));
        assert!(matches!(result[1].status, RepoStatus::Items(ref items) if items.is_empty()));
    }

    #[test]
    fn missing_author_becomes_ghost() {
        let chunk = vec![valid_repo(0, "acme/widget")];
        let envelope: GraphQlEnvelope = serde_json::from_value(serde_json::json!({
            "data": {
                "r0": {
                    "issues": { "totalCount": 1, "nodes": [
                        { "number": 1, "title": "orphan", "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z", "author": null }
                    ] },
                    "pullRequests": { "totalCount": 0, "nodes": [] }
                }
            }
        }))
        .unwrap();

        let result = parse_envelope(&chunk, envelope, None);
        let RepoStatus::Items(items) = &result[0].status else {
            panic!("expected items")
        };
        assert_eq!(items[0].author, "ghost");
    }

    #[test]
    fn overflow_beyond_page_size_requests_fallback() {
        let chunk = vec![valid_repo(0, "acme/busy")];
        let envelope: GraphQlEnvelope = serde_json::from_value(serde_json::json!({
            "data": {
                "r0": {
                    "issues": { "totalCount": 150, "nodes": [] },
                    "pullRequests": { "totalCount": 0, "nodes": [] }
                }
            }
        }))
        .unwrap();

        let result = parse_envelope(&chunk, envelope, None);
        assert!(result[0].needs_fallback);
    }

    #[test]
    fn subscribed_filter_narrows_items() {
        let chunk = vec![valid_repo(0, "acme/widget")];
        let envelope: GraphQlEnvelope = serde_json::from_value(serde_json::json!({
            "data": {
                "r0": {
                    "issues": { "totalCount": 2, "nodes": [
                        { "number": 1, "title": "a", "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z", "author": { "login": "alice" } },
                        { "number": 2, "title": "b", "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z", "author": { "login": "alice" } }
                    ] },
                    "pullRequests": { "totalCount": 0, "nodes": [] }
                }
            }
        }))
        .unwrap();

        let mut subscribed = SubscribedItems::new();
        subscribed.insert(
            "acme/widget".to_owned(),
            std::collections::HashSet::from([1]),
        );

        let result = parse_envelope(&chunk, envelope, Some(&subscribed));
        let RepoStatus::Items(items) = &result[0].status else {
            panic!("expected items")
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].number, 1);
    }

    #[test]
    fn non_not_found_error_maps_to_error_status() {
        let chunk = vec![valid_repo(0, "acme/forbidden")];
        let envelope: GraphQlEnvelope = serde_json::from_value(serde_json::json!({
            "data": { "r0": null },
            "errors": [
                { "type": "FORBIDDEN", "path": ["r0"], "message": "Resource protected by organization SAML enforcement" }
            ]
        }))
        .unwrap();

        let result = parse_envelope(&chunk, envelope, None);
        assert!(
            matches!(&result[0].status, RepoStatus::Error(RepoError::Api(m)) if m.contains("SAML"))
        );
    }
}
