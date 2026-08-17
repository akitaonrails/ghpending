use chrono::{DateTime, Utc};

use crate::github::{RepoResult, RepoStatus};

/// Repo ordering mode for the digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    /// Repos with the most recently updated open item first.
    Activity,
    /// The config-file order (repos are stored sorted alphabetically).
    Name,
    /// Most open items first.
    Count,
    /// Repos whose oldest-updated open item is oldest come first.
    Stale,
}

pub const SORT_NAMES: &[&str] = &["activity", "name", "count", "stale"];

impl SortMode {
    pub fn by_name(name: &str) -> Option<SortMode> {
        match name {
            "activity" => Some(SortMode::Activity),
            "name" => Some(SortMode::Name),
            "count" => Some(SortMode::Count),
            "stale" => Some(SortMode::Stale),
            _ => None,
        }
    }
}

/// Orders `results` in place per `mode`. `RepoStatus::Items` repos always
/// sort before `NotFound`/`Error` repos, which are pushed to the end,
/// preserving their relative order among themselves (this relies on
/// `sort_by`'s stability). Ties within the `Items` group always fall back
/// to case-insensitive repo name ascending.
pub fn sort_results(results: &mut [RepoResult], mode: SortMode) {
    results.sort_by(|a, b| {
        let key_a = group_key(a);
        let key_b = group_key(b);

        key_a.cmp(&key_b).then_with(|| match (key_a, key_b) {
            (0, 0) => items_cmp(a, b, mode),
            _ => std::cmp::Ordering::Equal,
        })
    });
}

/// 0 for `Items`, 1 for anything else (`NotFound`/`Error`), so the latter
/// always sorts to the end.
fn group_key(result: &RepoResult) -> u8 {
    match &result.status {
        RepoStatus::Items(_) => 0,
        RepoStatus::NotFound | RepoStatus::Error(_) => 1,
    }
}

fn name_cmp(a: &RepoResult, b: &RepoResult) -> std::cmp::Ordering {
    a.repo
        .to_ascii_lowercase()
        .cmp(&b.repo.to_ascii_lowercase())
}

fn items_cmp(a: &RepoResult, b: &RepoResult, mode: SortMode) -> std::cmp::Ordering {
    match mode {
        SortMode::Name => name_cmp(a, b),
        SortMode::Count => count_of(a)
            .cmp(&count_of(b))
            .reverse()
            .then_with(|| name_cmp(a, b)),
        SortMode::Activity => cmp_timestamps_none_last(max_updated(a), max_updated(b), false)
            .then_with(|| name_cmp(a, b)),
        SortMode::Stale => cmp_timestamps_none_last(min_updated(a), min_updated(b), true)
            .then_with(|| name_cmp(a, b)),
    }
}

/// Compares two optional timestamps, always sorting `None` (repos with no
/// items) after any `Some` value, regardless of `ascending`.
fn cmp_timestamps_none_last(
    a: Option<DateTime<Utc>>,
    b: Option<DateTime<Utc>>,
    ascending: bool,
) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => {
            if ascending {
                x.cmp(&y)
            } else {
                y.cmp(&x)
            }
        }
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn count_of(result: &RepoResult) -> usize {
    match &result.status {
        RepoStatus::Items(items) => items.len(),
        _ => 0,
    }
}

fn max_updated(result: &RepoResult) -> Option<DateTime<Utc>> {
    match &result.status {
        RepoStatus::Items(items) => items.iter().map(|item| item.updated_at).max(),
        _ => None,
    }
}

fn min_updated(result: &RepoResult) -> Option<DateTime<Utc>> {
    match &result.status {
        RepoStatus::Items(items) => items.iter().map(|item| item.updated_at).min(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{ItemKind, RepoError, RepoItem};

    /// A fixed instant (rather than `Utc::now()`) so timestamps built from it
    /// are exactly comparable — no flakiness from two `now()` calls landing
    /// on different nanoseconds.
    fn base_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn item(number: u64, updated_days_ago: i64) -> RepoItem {
        RepoItem {
            kind: ItemKind::Issue,
            number,
            title: format!("item {number}"),
            created_at: base_time(),
            updated_at: base_time() - chrono::Duration::days(updated_days_ago),
            author: "user".into(),
            pr_draft: None,
        }
    }

    fn items_repo(name: &str, updated_days_ago: &[i64]) -> RepoResult {
        RepoResult {
            repo: name.into(),
            status: RepoStatus::Items(
                updated_days_ago
                    .iter()
                    .enumerate()
                    .map(|(i, &days)| item(i as u64 + 1, days))
                    .collect(),
            ),
        }
    }

    fn empty_repo(name: &str) -> RepoResult {
        RepoResult {
            repo: name.into(),
            status: RepoStatus::Items(vec![]),
        }
    }

    fn error_repo(name: &str) -> RepoResult {
        RepoResult {
            repo: name.into(),
            status: RepoStatus::Error(RepoError::Timeout),
        }
    }

    fn not_found_repo(name: &str) -> RepoResult {
        RepoResult {
            repo: name.into(),
            status: RepoStatus::NotFound,
        }
    }

    fn names(results: &[RepoResult]) -> Vec<&str> {
        results.iter().map(|r| r.repo.as_str()).collect()
    }

    fn mixed_fixture() -> Vec<RepoResult> {
        vec![
            items_repo("z/stale-many", &[10, 20, 30]),
            items_repo("a/fresh-one", &[0]),
            error_repo("m/flaky"),
            empty_repo("e/empty"),
            not_found_repo("n/missing"),
            items_repo("b/mid", &[1, 2]),
        ]
    }

    #[test]
    fn by_name_recognizes_all_modes() {
        assert_eq!(SortMode::by_name("activity"), Some(SortMode::Activity));
        assert_eq!(SortMode::by_name("name"), Some(SortMode::Name));
        assert_eq!(SortMode::by_name("count"), Some(SortMode::Count));
        assert_eq!(SortMode::by_name("stale"), Some(SortMode::Stale));
        assert_eq!(SortMode::by_name("bogus"), None);
    }

    #[test]
    fn activity_orders_by_most_recently_updated_item_descending() {
        let mut results = mixed_fixture();
        sort_results(&mut results, SortMode::Activity);
        // items repos sorted by most recent update first; empty repo (no
        // items => None key) sorts after all items repos with items, then
        // errors/not-found go last.
        assert_eq!(
            names(&results),
            vec![
                "a/fresh-one",
                "b/mid",
                "z/stale-many",
                "e/empty",
                "m/flaky",
                "n/missing",
            ]
        );
    }

    #[test]
    fn name_orders_case_insensitive_alphabetically() {
        let mut results = mixed_fixture();
        sort_results(&mut results, SortMode::Name);
        assert_eq!(
            names(&results),
            vec![
                "a/fresh-one",
                "b/mid",
                "e/empty",
                "z/stale-many",
                "m/flaky",
                "n/missing",
            ]
        );
    }

    #[test]
    fn count_orders_by_item_count_descending_ties_by_name() {
        let mut results = mixed_fixture();
        sort_results(&mut results, SortMode::Count);
        assert_eq!(
            names(&results),
            vec![
                "z/stale-many",
                "b/mid",
                "a/fresh-one",
                "e/empty",
                "m/flaky",
                "n/missing",
            ]
        );
    }

    #[test]
    fn stale_orders_by_oldest_updated_item_ascending() {
        let mut results = mixed_fixture();
        sort_results(&mut results, SortMode::Stale);
        assert_eq!(
            names(&results),
            vec![
                "z/stale-many",
                "b/mid",
                "a/fresh-one",
                "e/empty",
                "m/flaky",
                "n/missing",
            ]
        );
    }

    #[test]
    fn ties_break_alphabetically_case_insensitive() {
        let mut results = vec![
            items_repo("B/repo", &[5]),
            items_repo("a/repo", &[5]),
            items_repo("C/repo", &[5]),
        ];
        sort_results(&mut results, SortMode::Activity);
        assert_eq!(names(&results), vec!["a/repo", "B/repo", "C/repo"]);
    }

    #[test]
    fn count_ties_break_alphabetically() {
        let mut results = vec![items_repo("z/two", &[1, 2]), items_repo("a/two", &[1, 2])];
        sort_results(&mut results, SortMode::Count);
        assert_eq!(names(&results), vec!["a/two", "z/two"]);
    }

    #[test]
    fn errors_and_not_found_always_go_last_preserving_relative_order() {
        for mode in [
            SortMode::Activity,
            SortMode::Name,
            SortMode::Count,
            SortMode::Stale,
        ] {
            let mut results = vec![
                error_repo("z/err-first"),
                items_repo("a/items", &[1]),
                not_found_repo("m/missing-second"),
            ];
            sort_results(&mut results, mode);
            assert_eq!(names(&results)[0], "a/items");
            assert_eq!(
                &names(&results)[1..],
                vec!["z/err-first", "m/missing-second"],
                "mode {mode:?} should preserve relative order of failed repos"
            );
        }
    }

    #[test]
    fn empty_items_repo_sorts_after_repos_with_items() {
        let mut results = vec![empty_repo("a/empty"), items_repo("z/has-items", &[1])];
        sort_results(&mut results, SortMode::Activity);
        assert_eq!(names(&results), vec!["z/has-items", "a/empty"]);
    }

    #[test]
    fn empty_items_repo_sorts_after_repos_with_items_in_stale_mode() {
        let mut results = vec![empty_repo("a/empty"), items_repo("z/has-items", &[1])];
        sort_results(&mut results, SortMode::Stale);
        assert_eq!(names(&results), vec!["z/has-items", "a/empty"]);
    }
}
