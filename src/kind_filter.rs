use crate::github::{ItemKind, RepoResult, RepoStatus};

/// Which item kinds the digest shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindFilter {
    /// Pull requests, issues and (for repos you own) alerts.
    All,
    /// Pull requests only.
    PullRequests,
    /// Issues only.
    Issues,
}

pub const KIND_NAMES: &[&str] = &["all", "prs", "issues"];

impl KindFilter {
    pub fn by_name(name: &str) -> Option<KindFilter> {
        match name {
            "all" => Some(KindFilter::All),
            "prs" => Some(KindFilter::PullRequests),
            "issues" => Some(KindFilter::Issues),
            _ => None,
        }
    }

    /// Alerts are their own kind (`SEC`/`QUA`), so narrowing the digest to
    /// PRs or issues leaves them out — and skips fetching them at all.
    pub fn includes_alerts(self) -> bool {
        self == KindFilter::All
    }

    fn keeps(self, kind: &ItemKind) -> bool {
        match self {
            KindFilter::All => true,
            KindFilter::PullRequests => *kind == ItemKind::PullRequest,
            KindFilter::Issues => *kind == ItemKind::Issue,
        }
    }
}

/// Drops items whose kind `filter` excludes, in place. Repos left with no
/// items are kept as `Items(vec![])`, which the digest already omits while
/// still counting them as checked.
pub fn apply(results: &mut [RepoResult], filter: KindFilter) {
    if filter == KindFilter::All {
        return;
    }
    for result in results.iter_mut() {
        if let RepoStatus::Items(items) = &mut result.status {
            items.retain(|item| filter.keeps(&item.kind));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::RepoItem;

    fn item(kind: ItemKind, number: u64) -> RepoItem {
        RepoItem {
            kind,
            number,
            title: format!("item {number}"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            author: "alice".into(),
            pr_draft: None,
            comments: None,
            review_decision: None,
            mine: false,
            severity: None,
            source: None,
        }
    }

    fn sample() -> Vec<RepoResult> {
        vec![
            RepoResult::new(
                "a/b".into(),
                RepoStatus::Items(vec![
                    item(ItemKind::Security, 9),
                    item(ItemKind::PullRequest, 3),
                    item(ItemKind::Issue, 2),
                    item(ItemKind::Quality, 8),
                ]),
            ),
            RepoResult::new(
                "c/d".into(),
                RepoStatus::Items(vec![item(ItemKind::Issue, 1)]),
            ),
            RepoResult::new("e/f".into(), RepoStatus::NotFound),
        ]
    }

    fn numbers(result: &RepoResult) -> Vec<u64> {
        match &result.status {
            RepoStatus::Items(items) => items.iter().map(|i| i.number).collect(),
            _ => panic!("expected items"),
        }
    }

    #[test]
    fn by_name_recognizes_all_kinds() {
        assert_eq!(KindFilter::by_name("all"), Some(KindFilter::All));
        assert_eq!(KindFilter::by_name("prs"), Some(KindFilter::PullRequests));
        assert_eq!(KindFilter::by_name("issues"), Some(KindFilter::Issues));
        assert_eq!(KindFilter::by_name("pr"), None);
    }

    #[test]
    fn all_leaves_results_untouched() {
        let mut results = sample();
        apply(&mut results, KindFilter::All);
        assert_eq!(numbers(&results[0]), vec![9, 3, 2, 8]);
        assert_eq!(numbers(&results[1]), vec![1]);
    }

    #[test]
    fn prs_keeps_only_pull_requests_and_empties_issue_only_repos() {
        let mut results = sample();
        apply(&mut results, KindFilter::PullRequests);
        assert_eq!(numbers(&results[0]), vec![3]);
        assert!(numbers(&results[1]).is_empty());
        assert!(matches!(results[2].status, RepoStatus::NotFound));
    }

    #[test]
    fn issues_keeps_only_issues() {
        let mut results = sample();
        apply(&mut results, KindFilter::Issues);
        assert_eq!(numbers(&results[0]), vec![2]);
        assert_eq!(numbers(&results[1]), vec![1]);
    }

    #[test]
    fn only_all_includes_alerts() {
        assert!(KindFilter::All.includes_alerts());
        assert!(!KindFilter::PullRequests.includes_alerts());
        assert!(!KindFilter::Issues.includes_alerts());
    }
}
