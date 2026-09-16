use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::stream::{FuturesUnordered, StreamExt};
use octocrab::Octocrab;
use serde::Deserialize;
use tokio::time::timeout;

use crate::github::{
    self, ForkCache, ItemKind, RepoError, RepoItem, RepoResult, RepoStatus, SubscribedItems,
    item_cmp, retain_subscribed, split_repo,
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
    #[serde(default, rename = "isFork")]
    is_fork: bool,
    parent: Option<Parent>,
}

#[derive(Debug, Deserialize)]
struct Parent {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
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
/// page (needs a follow-up exhaustive REST fetch for full parity), and
/// whether it's a fork with a known parent (redirects to the upstream
/// search query instead).
struct ChunkItem {
    original_index: usize,
    repo: String,
    status: RepoStatus,
    needs_fallback: bool,
    fork_parent: Option<String>,
}

/// A tracked repo detected as a fork, awaiting the upstream-items search
/// query.
struct ForkTarget {
    original_index: usize,
    repo: String,
    parent: String,
    fork_owner: String,
}

pub async fn fetch_repos_batched(
    crab: &Octocrab,
    repos: &[String],
    subscribed: Option<&SubscribedItems>,
    fork_overrides: &ForkCache,
) -> (Vec<RepoResult>, ForkCache) {
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
                results[index] = Some(RepoResult::new(repo.clone(), RepoStatus::NotFound));
            }
        }
    }

    let chunks: Vec<&[ValidRepo]> = valid.chunks(CHUNK_SIZE).collect();
    let mut overflow: Vec<(usize, String)> = Vec::new();
    let mut fork_targets: Vec<ForkTarget> = Vec::new();
    let mut fork_cache: ForkCache = ForkCache::new();

    let mut in_flight = FuturesUnordered::new();
    let mut next_chunk = 0;

    while next_chunk < chunks.len() && in_flight.len() < MAX_CONCURRENT_CHUNKS {
        in_flight.push(fetch_chunk(crab, chunks[next_chunk], subscribed));
        next_chunk += 1;
    }

    while let Some(chunk_items) = in_flight.next().await {
        for item in chunk_items {
            if matches!(item.status, RepoStatus::Items(_)) {
                // Refresh the cache entry for every repo we got a definitive
                // answer for — but a manual entry in the config always wins,
                // so an opt-out ("" for a repo that IS a fork) sticks.
                let cache_value = fork_overrides
                    .get(item.repo.as_str())
                    .cloned()
                    .unwrap_or_else(|| item.fork_parent.clone().unwrap_or_default());
                fork_cache.insert(item.repo.clone(), cache_value);
            }

            let effective_parent = effective_fork_parent(
                fork_overrides.get(item.repo.as_str()),
                item.fork_parent.clone(),
            );
            if let Some(parent) = effective_parent {
                let fork_owner =
                    split_repo(&item.repo).map_or_else(String::new, |(owner, _)| owner.to_owned());
                fork_targets.push(ForkTarget {
                    original_index: item.original_index,
                    repo: item.repo,
                    parent,
                    fork_owner,
                });
                continue;
            }

            if item.needs_fallback {
                overflow.push((item.original_index, item.repo.clone()));
            }
            results[item.original_index] = Some(RepoResult::new(item.repo, item.status));
        }
        if next_chunk < chunks.len() {
            in_flight.push(fetch_chunk(crab, chunks[next_chunk], subscribed));
            next_chunk += 1;
        }
    }

    if !overflow.is_empty() {
        let overflow_repos: Vec<String> = overflow.iter().map(|(_, repo)| repo.clone()).collect();
        // Overflow repos are known non-forks this run (fork items never take
        // the fallback), so hand REST a cache that says so — otherwise it
        // would burn one detection GET per repo re-checking.
        let overflow_cache: ForkCache = overflow_repos
            .iter()
            .map(|repo| (repo.clone(), String::new()))
            .collect();
        let (overflow_results, _) =
            github::fetch_repos_rest(crab, &overflow_repos, subscribed, &overflow_cache).await;
        for ((original_index, _), result) in overflow.into_iter().zip(overflow_results) {
            results[original_index] = Some(result);
        }
    }

    if !fork_targets.is_empty() {
        let fork_results = fetch_fork_items_batched(crab, &fork_targets).await;
        for (target, result) in fork_targets.into_iter().zip(fork_results) {
            results[target.original_index] = Some(result);
        }
    }

    let results = results
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            result.unwrap_or_else(|| {
                RepoResult::new(repos[index].clone(), RepoStatus::Error(RepoError::Timeout))
            })
        })
        .collect();

    (results, fork_cache)
}

/// Resolves which upstream (if any) a repo's digest should target, giving a
/// manual config entry priority over live detection: `""` opts a real fork
/// out of the upstream view, a non-empty value forces/overrides the
/// upstream, and no entry defers to what GitHub reported.
fn effective_fork_parent(
    override_value: Option<&String>,
    detected: Option<String>,
) -> Option<String> {
    match override_value {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(value.clone()),
        None => detected,
    }
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
        Ok(Err(e)) => return error_for_all(chunk, RepoError::Api(github::describe_api_error(&e))),
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
            fork_parent: None,
        })
        .collect()
}

fn build_query(chunk: &[ValidRepo<'_>]) -> String {
    let mut query = String::from("query {");
    for (i, repo) in chunk.iter().enumerate() {
        query.push_str(&format!(
            "\n  r{i}: repository(owner: {:?}, name: {:?}) {{\n    isFork\n    parent {{ nameWithOwner }}\n    issues(states: OPEN, first: {MAX_ITEMS_PER_CONNECTION}) {{ totalCount nodes {{ number title createdAt updatedAt author {{ login }} }} }}\n    pullRequests(states: OPEN, first: {MAX_ITEMS_PER_CONNECTION}) {{ totalCount nodes {{ number title createdAt updatedAt author {{ login }} isDraft }} }}\n  }}",
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
    let has_errors = envelope
        .errors
        .as_ref()
        .is_some_and(|errors| !errors.is_empty());
    if envelope.data.is_none() && !has_errors {
        return error_for_all(chunk, RepoError::Api("empty GraphQL response".to_owned()));
    }

    let mut not_found: HashMap<String, ()> = HashMap::new();
    let mut errored: HashMap<String, String> = HashMap::new();
    // Errors without a `path` (rate limiting, query complexity, auth-level
    // failures) apply to the whole chunk, not a single repo alias — every
    // repo would otherwise fall through to a misleading NotFound.
    let mut pathless_messages: Vec<String> = Vec::new();

    for error in envelope.errors.into_iter().flatten() {
        let Some(alias) = error
            .path
            .as_ref()
            .and_then(|p| p.first())
            .and_then(|v| v.as_str())
        else {
            pathless_messages.push(error.message.clone());
            continue;
        };
        let alias = alias.to_owned();
        if error.error_type.as_deref() == Some("NOT_FOUND") {
            not_found.insert(alias, ());
        } else {
            errored.entry(alias).or_insert(error.message.clone());
        }
    }

    if !pathless_messages.is_empty() {
        return error_for_all(chunk, RepoError::Api(pathless_messages.join("; ")));
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
                    fork_parent: None,
                };
            }
            if let Some(message) = errored.get(alias.as_str()) {
                return ChunkItem {
                    original_index: repo.original_index,
                    repo: repo.repo.to_owned(),
                    status: RepoStatus::Error(RepoError::Api(message.clone())),
                    needs_fallback: false,
                    fork_parent: None,
                };
            }

            match data.get(&alias) {
                Some(Some(node)) => {
                    let needs_fallback = node.issues.total_count > MAX_ITEMS_PER_CONNECTION
                        || node.pull_requests.total_count > MAX_ITEMS_PER_CONNECTION;
                    let mut items = items_from_node(node);
                    retain_subscribed(&mut items, subscribed_numbers);
                    items.sort_by(item_cmp);
                    let fork_parent = if node.is_fork {
                        node.parent.as_ref().map(|p| p.name_with_owner.clone())
                    } else {
                        None
                    };
                    ChunkItem {
                        original_index: repo.original_index,
                        repo: repo.repo.to_owned(),
                        status: RepoStatus::Items(items),
                        needs_fallback,
                        fork_parent,
                    }
                }
                _ => ChunkItem {
                    original_index: repo.original_index,
                    repo: repo.repo.to_owned(),
                    status: RepoStatus::NotFound,
                    needs_fallback: false,
                    fork_parent: None,
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
            comments: None,
            review_decision: None,
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
            comments: None,
            review_decision: None,
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

#[derive(Debug, Deserialize, Default)]
struct SearchEnvelope {
    data: Option<HashMap<String, Option<SearchConnection>>>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Debug, Deserialize)]
struct SearchConnection {
    nodes: Vec<SearchNode>,
}

#[derive(Debug, Deserialize)]
struct SearchNode {
    #[serde(rename = "__typename")]
    typename: String,
    number: Option<u64>,
    title: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<DateTime<Utc>>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<DateTime<Utc>>,
    author: Option<Author>,
    comments: Option<CommentsCount>,
    #[serde(default, rename = "isDraft")]
    is_draft: Option<bool>,
    #[serde(default, rename = "reviewDecision")]
    review_decision: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommentsCount {
    #[serde(rename = "totalCount")]
    total_count: u64,
}

/// Fetches upstream issues/PRs for every detected fork, chunked and
/// throttled the same way as the repo query. Results come back in the same
/// order as `targets`.
async fn fetch_fork_items_batched(crab: &Octocrab, targets: &[ForkTarget]) -> Vec<RepoResult> {
    let indexed: Vec<(usize, &ForkTarget)> = targets.iter().enumerate().collect();
    let chunks: Vec<&[(usize, &ForkTarget)]> = indexed.chunks(CHUNK_SIZE).collect();
    let mut results: Vec<Option<RepoResult>> = vec![None; targets.len()];

    let mut in_flight = FuturesUnordered::new();
    let mut next_chunk = 0;

    while next_chunk < chunks.len() && in_flight.len() < MAX_CONCURRENT_CHUNKS {
        in_flight.push(fetch_fork_chunk(crab, chunks[next_chunk]));
        next_chunk += 1;
    }

    while let Some(chunk_results) = in_flight.next().await {
        for (local_index, result) in chunk_results {
            results[local_index] = Some(result);
        }
        if next_chunk < chunks.len() {
            in_flight.push(fetch_fork_chunk(crab, chunks[next_chunk]));
            next_chunk += 1;
        }
    }

    results
        .into_iter()
        .enumerate()
        .map(|(i, result)| {
            result.unwrap_or_else(|| {
                RepoResult::new(
                    targets[i].repo.clone(),
                    RepoStatus::Error(RepoError::Timeout),
                )
                .with_upstream(targets[i].parent.clone())
            })
        })
        .collect()
}

async fn fetch_fork_chunk(
    crab: &Octocrab,
    chunk: &[(usize, &ForkTarget)],
) -> Vec<(usize, RepoResult)> {
    let query = build_search_query(chunk);
    let body = serde_json::json!({ "query": query });

    let response = match timeout(
        CHUNK_TIMEOUT,
        crab.post::<_, SearchEnvelope>("/graphql", Some(&body)),
    )
    .await
    {
        Ok(Ok(envelope)) => envelope,
        Ok(Err(e)) => {
            return search_error_for_all(chunk, RepoError::Api(github::describe_api_error(&e)));
        }
        Err(_) => return search_error_for_all(chunk, RepoError::Timeout),
    };

    parse_search_envelope(chunk, response)
}

fn search_error_for_all(
    chunk: &[(usize, &ForkTarget)],
    error: RepoError,
) -> Vec<(usize, RepoResult)> {
    chunk
        .iter()
        .map(|(local_index, target)| {
            (
                *local_index,
                RepoResult::new(target.repo.clone(), RepoStatus::Error(error.clone()))
                    .with_upstream(target.parent.clone()),
            )
        })
        .collect()
}

fn build_search_query(chunk: &[(usize, &ForkTarget)]) -> String {
    let mut query = String::from("query {");
    for (i, (_, target)) in chunk.iter().enumerate() {
        let search_query = format!(
            "repo:{} author:{} is:open",
            target.parent, target.fork_owner
        );
        query.push_str(&format!(
            "\n  s{i}: search(query: {search_query:?}, type: ISSUE, first: {MAX_ITEMS_PER_CONNECTION}) {{ nodes {{ __typename ... on Issue {{ number title createdAt updatedAt author {{ login }} comments {{ totalCount }} }} ... on PullRequest {{ number title createdAt updatedAt author {{ login }} comments {{ totalCount }} isDraft reviewDecision }} }} }}"
        ));
    }
    query.push_str("\n}");
    query
}

fn parse_search_envelope(
    chunk: &[(usize, &ForkTarget)],
    envelope: SearchEnvelope,
) -> Vec<(usize, RepoResult)> {
    let has_errors = envelope
        .errors
        .as_ref()
        .is_some_and(|errors| !errors.is_empty());
    if envelope.data.is_none() && !has_errors {
        return search_error_for_all(chunk, RepoError::Api("empty GraphQL response".to_owned()));
    }

    let mut errored: HashMap<String, String> = HashMap::new();
    let mut pathless_messages: Vec<String> = Vec::new();

    for error in envelope.errors.into_iter().flatten() {
        let Some(alias) = error
            .path
            .as_ref()
            .and_then(|p| p.first())
            .and_then(|v| v.as_str())
        else {
            pathless_messages.push(error.message.clone());
            continue;
        };
        errored.entry(alias.to_owned()).or_insert(error.message);
    }

    if !pathless_messages.is_empty() {
        return search_error_for_all(chunk, RepoError::Api(pathless_messages.join("; ")));
    }

    let data = envelope.data.unwrap_or_default();

    chunk
        .iter()
        .enumerate()
        .map(|(i, (local_index, target))| {
            let alias = format!("s{i}");
            if let Some(message) = errored.get(alias.as_str()) {
                return (
                    *local_index,
                    RepoResult::new(
                        target.repo.clone(),
                        RepoStatus::Error(RepoError::Api(message.clone())),
                    )
                    .with_upstream(target.parent.clone()),
                );
            }

            let items = match data.get(&alias) {
                Some(Some(connection)) => items_from_search_nodes(&connection.nodes),
                _ => Vec::new(),
            };
            (
                *local_index,
                RepoResult::new(target.repo.clone(), RepoStatus::Items(items))
                    .with_upstream(target.parent.clone()),
            )
        })
        .collect()
}

fn items_from_search_nodes(nodes: &[SearchNode]) -> Vec<RepoItem> {
    let mut items = Vec::with_capacity(nodes.len());

    for node in nodes {
        let Some(number) = node.number else { continue };
        let kind = if node.typename == "PullRequest" {
            ItemKind::PullRequest
        } else {
            ItemKind::Issue
        };
        let created_at = node.created_at.unwrap_or_else(Utc::now);
        let updated_at = node.updated_at.unwrap_or(created_at);
        let comments = node.comments.as_ref().map(|c| c.total_count);
        let (pr_draft, review_decision) = if kind == ItemKind::PullRequest {
            (
                node.is_draft,
                map_review_decision(node.review_decision.as_deref()),
            )
        } else {
            (None, None)
        };

        items.push(RepoItem {
            kind,
            number,
            title: node.title.clone().unwrap_or_default(),
            created_at,
            updated_at,
            author: author_login(&node.author),
            pr_draft,
            comments,
            review_decision,
        });
    }

    items.sort_by(item_cmp);
    items
}

fn map_review_decision(raw: Option<&str>) -> Option<String> {
    match raw {
        Some("APPROVED") => Some("approved".to_owned()),
        Some("CHANGES_REQUESTED") => Some("changes requested".to_owned()),
        Some("REVIEW_REQUIRED") => Some("review pending".to_owned()),
        _ => None,
    }
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
    fn effective_fork_parent_honors_overrides() {
        let opt_out = String::new();
        let custom = "someone/else".to_owned();
        // "" opts a detected fork out of the upstream view.
        assert_eq!(
            effective_fork_parent(Some(&opt_out), Some("up/stream".into())),
            None
        );
        // A non-empty manual entry wins over detection.
        assert_eq!(
            effective_fork_parent(Some(&custom), Some("up/stream".into())),
            Some("someone/else".into())
        );
        // No entry defers to detection, either way.
        assert_eq!(
            effective_fork_parent(None, Some("up/stream".into())),
            Some("up/stream".into())
        );
        assert_eq!(effective_fork_parent(None, None), None);
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
    fn pathless_error_marks_every_repo_in_chunk_as_error_not_not_found() {
        let chunk = vec![valid_repo(0, "acme/widget"), valid_repo(1, "acme/gadget")];
        let envelope: GraphQlEnvelope = serde_json::from_value(serde_json::json!({
            "errors": [
                { "type": "RATE_LIMITED", "message": "API rate limit exceeded" }
            ]
        }))
        .unwrap();

        let result = parse_envelope(&chunk, envelope, None);
        assert_eq!(result.len(), 2);
        for item in &result {
            assert!(
                matches!(&item.status, RepoStatus::Error(RepoError::Api(m)) if m.contains("rate limit")),
                "expected rate-limit error, got {:?}",
                item.status
            );
            assert!(!matches!(item.status, RepoStatus::NotFound));
        }
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

    #[test]
    fn fork_with_parent_is_detected_and_excluded_from_fallback() {
        let chunk = vec![valid_repo(0, "akitaonrails/omarchy")];
        let envelope: GraphQlEnvelope = serde_json::from_value(serde_json::json!({
            "data": {
                "r0": {
                    "isFork": true,
                    "parent": { "nameWithOwner": "omacom/omarchy" },
                    "issues": { "totalCount": 150, "nodes": [] },
                    "pullRequests": { "totalCount": 0, "nodes": [] }
                }
            }
        }))
        .unwrap();

        let result = parse_envelope(&chunk, envelope, None);
        assert_eq!(result[0].fork_parent.as_deref(), Some("omacom/omarchy"));
        // Even though the fork's own issues overflow the page, the caller
        // discards them for the search-based upstream fetch instead.
        assert!(result[0].needs_fallback);
    }

    #[test]
    fn non_fork_repo_has_no_fork_parent() {
        let chunk = vec![valid_repo(0, "acme/widget")];
        let envelope: GraphQlEnvelope = serde_json::from_value(serde_json::json!({
            "data": {
                "r0": {
                    "isFork": false,
                    "issues": { "totalCount": 0, "nodes": [] },
                    "pullRequests": { "totalCount": 0, "nodes": [] }
                }
            }
        }))
        .unwrap();

        let result = parse_envelope(&chunk, envelope, None);
        assert_eq!(result[0].fork_parent, None);
    }

    fn fork_target(local_index: usize, repo: &str, parent: &str) -> ForkTarget {
        let (owner, _) = split_repo(repo).unwrap();
        ForkTarget {
            original_index: local_index,
            repo: repo.to_owned(),
            parent: parent.to_owned(),
            fork_owner: owner.to_owned(),
        }
    }

    #[test]
    fn build_search_query_aliases_targets_with_repo_and_author_filters() {
        let target = fork_target(0, "akitaonrails/omarchy", "omacom/omarchy");
        let chunk: Vec<(usize, &ForkTarget)> = vec![(0, &target)];
        let query = build_search_query(&chunk);
        assert!(query.contains("s0: search(query:"));
        assert!(query.contains("repo:omacom/omarchy author:akitaonrails is:open"));
        assert!(query.contains("type: ISSUE"));
        assert!(query.contains("... on Issue"));
        assert!(query.contains("... on PullRequest"));
        assert!(query.contains("reviewDecision"));
    }

    #[test]
    fn parse_search_envelope_maps_issue_and_pr_with_comments_and_review_decision() {
        let target = fork_target(5, "akitaonrails/omarchy", "omacom/omarchy");
        let chunk: Vec<(usize, &ForkTarget)> = vec![(5, &target)];
        let envelope: SearchEnvelope = serde_json::from_value(serde_json::json!({
            "data": {
                "s0": { "nodes": [
                    { "__typename": "Issue", "number": 3, "title": "bug", "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-02T00:00:00Z", "author": { "login": "akitaonrails" }, "comments": { "totalCount": 2 } },
                    { "__typename": "PullRequest", "number": 7, "title": "feature", "createdAt": "2026-01-03T00:00:00Z", "updatedAt": "2026-01-04T00:00:00Z", "author": { "login": "akitaonrails" }, "comments": { "totalCount": 5 }, "isDraft": false, "reviewDecision": "CHANGES_REQUESTED" }
                ] }
            }
        }))
        .unwrap();

        let result = parse_search_envelope(&chunk, envelope);
        assert_eq!(result.len(), 1);
        let (local_index, repo_result) = &result[0];
        assert_eq!(*local_index, 5);
        assert_eq!(repo_result.upstream.as_deref(), Some("omacom/omarchy"));
        let RepoStatus::Items(items) = &repo_result.status else {
            panic!("expected items")
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, ItemKind::PullRequest);
        assert_eq!(items[0].comments, Some(5));
        assert_eq!(
            items[0].review_decision.as_deref(),
            Some("changes requested")
        );
        assert_eq!(items[1].kind, ItemKind::Issue);
        assert_eq!(items[1].comments, Some(2));
        assert_eq!(items[1].review_decision, None);
    }

    #[test]
    fn map_review_decision_covers_known_values() {
        assert_eq!(
            map_review_decision(Some("APPROVED")).as_deref(),
            Some("approved")
        );
        assert_eq!(
            map_review_decision(Some("CHANGES_REQUESTED")).as_deref(),
            Some("changes requested")
        );
        assert_eq!(
            map_review_decision(Some("REVIEW_REQUIRED")).as_deref(),
            Some("review pending")
        );
        assert_eq!(map_review_decision(None), None);
        assert_eq!(map_review_decision(Some("SOMETHING_ELSE")), None);
    }
}
