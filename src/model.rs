//! Provider-neutral model shared by every backend (GitHub, GitLab, …).
//!
//! Fetchers translate their provider's API into these types; rendering and
//! ordering below this line know nothing about where an item came from.

use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct RepoItem {
    pub kind: ItemKind,
    pub number: u64,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub author: String,
    pub pr_draft: Option<bool>,
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
    Error(RepoError),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RepoError {
    #[error("timeout after 30s")]
    Timeout,
    #[error("{0}")]
    Api(String),
}

pub fn item_cmp(a: &RepoItem, b: &RepoItem) -> Ordering {
    match (&a.kind, &b.kind) {
        (ItemKind::PullRequest, ItemKind::Issue) => Ordering::Less,
        (ItemKind::Issue, ItemKind::PullRequest) => Ordering::Greater,
        _ => b.number.cmp(&a.number),
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
            author: "user".into(),
            pr_draft: None,
        }
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
