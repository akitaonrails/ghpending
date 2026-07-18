use chrono::Utc;
use owo_colors::Style;
use terminal_size::{Width, terminal_size};

use crate::format::{relative_time, truncate_title};
use crate::model::{ItemKind, RepoResult, RepoStatus};
use crate::theme::Theme;

fn term_width() -> usize {
    terminal_size().map_or(80, |(Width(w), _)| w as usize)
}

fn should_colorize() -> bool {
    std::env::var("NO_COLOR").is_err()
}

fn paint(text: &str, color: bool, style: Style) -> String {
    if color {
        style.style(text).to_string()
    } else {
        text.to_owned()
    }
}

pub fn render_digest(results: &[RepoResult], theme: &Theme) -> String {
    render_inner(results, theme, should_colorize(), term_width())
}

fn render_inner(results: &[RepoResult], theme: &Theme, color: bool, width: usize) -> String {
    if results.is_empty() {
        return "No repos tracked. Run `ghpending add` to get started.\n".into();
    }

    let now = Utc::now();
    let total = results.len();
    let with_pending = results
        .iter()
        .filter(|r| matches!(&r.status, RepoStatus::Items(items) if !items.is_empty()))
        .count();
    let failures = results
        .iter()
        .filter(|r| matches!(&r.status, RepoStatus::Error(_)))
        .count();

    let mut body = String::new();
    let mut shown = 0;

    for result in results {
        if matches!(&result.status, RepoStatus::Items(items) if items.is_empty()) {
            continue;
        }

        if shown > 0 {
            body.push('\n');
        }
        shown += 1;

        let repo_colored = paint(&result.repo, color, theme.repo);
        body.push_str(&format!("━━ {repo_colored}\n"));

        match &result.status {
            RepoStatus::NotFound => {
                let msg = paint("(not found or no access)", color, theme.meta);
                body.push_str(&format!("  {msg}\n"));
            }
            RepoStatus::Error(e) => {
                let msg = paint(&format!("(error: {e})"), color, theme.error);
                body.push_str(&format!("  {msg}\n"));
            }
            RepoStatus::Items(items) => {
                let title_max = if width > 20 { width - 20 } else { 10 };

                for item in items {
                    let (kind_str, number_str, title_str) = match item.kind {
                        ItemKind::PullRequest => {
                            let ks = paint("PR ", color, theme.pr);
                            let ns = format!("#{}", item.number);
                            let title = truncate_title(&item.title, title_max);
                            (ks, ns, title)
                        }
                        ItemKind::Issue => {
                            let ks = paint("ISS", color, theme.issue);
                            let ns = format!("#{}", item.number);
                            let title = truncate_title(&item.title, title_max);
                            (ks, ns, title)
                        }
                    };

                    body.push_str(&format!("  {kind_str}  {number_str}  {title_str}\n"));

                    let rel = relative_time(&item.created_at, &now);
                    let mut meta = format!("opened {} ago by {}", rel, item.author);
                    if let Some(state) = pr_state_label(item) {
                        meta.push_str(" · ");
                        meta.push_str(state);
                    }
                    let meta_colored = paint(&meta, color, theme.meta);
                    body.push_str(&format!("        {meta_colored}\n"));
                }
            }
        }
    }

    let summary = if failures > 0 {
        format!("{total} projects attempted, {with_pending} with pending tasks, {failures} failed")
    } else {
        format!("{total} projects checked, {with_pending} with pending tasks")
    };
    let summary_colored = paint(&summary, color, theme.meta);

    if body.is_empty() {
        format!("{summary_colored}\n")
    } else {
        format!("{summary_colored}\n\n{body}")
    }
}

fn pr_state_label(item: &crate::model::RepoItem) -> Option<&'static str> {
    match item.kind {
        ItemKind::PullRequest => match item.pr_draft {
            Some(true) => Some("draft"),
            Some(false) => Some("ready"),
            None => None,
        },
        ItemKind::Issue => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKind, RepoError, RepoItem, RepoResult, RepoStatus};
    use crate::theme::Theme;

    fn make_item(kind: ItemKind, number: u64, title: &str, days_ago: i64) -> RepoItem {
        let created_at = Utc::now() - chrono::Duration::days(days_ago);
        RepoItem {
            kind,
            number,
            title: title.into(),
            created_at,
            author: "testuser".into(),
            pr_draft: None,
        }
    }

    fn make_pr(number: u64, title: &str, draft: Option<bool>) -> RepoItem {
        RepoItem {
            kind: ItemKind::PullRequest,
            number,
            title: title.into(),
            created_at: Utc::now(),
            author: "testuser".into(),
            pr_draft: draft,
        }
    }

    #[test]
    fn no_repos_tracked() {
        let out = render_inner(&[], &Theme::default_theme(), false, 80);
        assert!(out.contains("ghpending add"));
    }

    #[test]
    fn empty_repo_is_skipped_from_listing() {
        let results = vec![RepoResult {
            repo: "owner/empty".into(),
            status: RepoStatus::Items(vec![]),
        }];
        let out = render_inner(&results, &Theme::default_theme(), false, 80);
        assert!(!out.contains("owner/empty"));
        assert!(!out.contains("nothing pending"));
        assert!(out.contains("1 projects checked, 0 with pending tasks"));
    }

    #[test]
    fn repo_not_found() {
        let results = vec![RepoResult {
            repo: "owner/missing".into(),
            status: RepoStatus::NotFound,
        }];
        let out = render_inner(&results, &Theme::default_theme(), false, 80);
        assert!(out.contains("owner/missing"));
        assert!(out.contains("not found or no access"));
    }

    #[test]
    fn repo_error_renders_message() {
        let results = vec![RepoResult {
            repo: "owner/flaky".into(),
            status: RepoStatus::Error(RepoError::Api("rate limited".into())),
        }];
        let out = render_inner(&results, &Theme::default_theme(), false, 80);
        assert!(out.contains("owner/flaky"));
        assert!(out.contains("error:"));
        assert!(out.contains("rate limited"));
    }

    #[test]
    fn normal_items_rendered() {
        let results = vec![RepoResult {
            repo: "ratatui-org/ratatui".into(),
            status: RepoStatus::Items(vec![
                make_item(ItemKind::PullRequest, 1842, "Fix overflow in Table", 2),
                make_item(ItemKind::Issue, 1840, "Crash on empty Paragraph", 0),
            ]),
        }];
        let out = render_inner(&results, &Theme::default_theme(), false, 80);
        assert!(out.contains("ratatui-org/ratatui"));
        assert!(out.contains("PR"));
        assert!(out.contains("#1842"));
        assert!(out.contains("Fix overflow in Table"));
        assert!(out.contains("ISS"));
        assert!(out.contains("#1840"));
        assert!(out.contains("testuser"));
    }

    #[test]
    fn pull_request_draft_status_is_rendered_subtly() {
        let results = vec![RepoResult {
            repo: "owner/repo".into(),
            status: RepoStatus::Items(vec![
                make_pr(3, "Work in progress", Some(true)),
                make_pr(2, "Ready for review", Some(false)),
                make_pr(1, "Unknown state", None),
            ]),
        }];

        let out = render_inner(&results, &Theme::default_theme(), false, 80);
        assert!(out.contains("opened just now ago by testuser · draft"));
        assert!(out.contains("opened just now ago by testuser · ready"));
        assert!(out.contains("opened just now ago by testuser\n"));
    }

    #[test]
    fn header_is_just_prefix_and_name() {
        let results = vec![RepoResult {
            repo: "a/b".into(),
            status: RepoStatus::Items(vec![make_item(ItemKind::Issue, 1, "x", 0)]),
        }];
        let out = render_inner(&results, &Theme::default_theme(), false, 80);
        let header_line = out.lines().find(|l| l.contains("━━")).unwrap();
        assert_eq!(header_line, "━━ a/b");
    }

    #[test]
    fn two_repos_separated_by_blank_line() {
        let results = vec![
            RepoResult {
                repo: "a/b".into(),
                status: RepoStatus::Items(vec![make_item(ItemKind::Issue, 1, "x", 0)]),
            },
            RepoResult {
                repo: "c/d".into(),
                status: RepoStatus::Items(vec![make_item(ItemKind::Issue, 2, "y", 0)]),
            },
        ];
        let out = render_inner(&results, &Theme::default_theme(), false, 80);
        let body = out.split_once("\n\n").unwrap().1;
        assert!(body.contains("\n\n"));
    }

    #[test]
    fn summary_counts_only_repos_with_items() {
        let results = vec![
            RepoResult {
                repo: "a/empty".into(),
                status: RepoStatus::Items(vec![]),
            },
            RepoResult {
                repo: "a/withitems".into(),
                status: RepoStatus::Items(vec![make_item(ItemKind::Issue, 1, "x", 0)]),
            },
            RepoResult {
                repo: "a/missing".into(),
                status: RepoStatus::NotFound,
            },
            RepoResult {
                repo: "a/flaky".into(),
                status: RepoStatus::Error(RepoError::Api("rate limited".into())),
            },
        ];
        let out = render_inner(&results, &Theme::default_theme(), false, 80);
        assert!(out.contains("4 projects attempted, 1 with pending tasks, 1 failed"));
        assert!(!out.contains("a/empty"));
        assert!(out.contains("a/withitems"));
        assert!(out.contains("a/missing"));
        assert!(out.contains("a/flaky"));
    }

    #[test]
    fn summary_counts_failures_when_no_pending_tasks() {
        let results = vec![
            RepoResult {
                repo: "a/empty".into(),
                status: RepoStatus::Items(vec![]),
            },
            RepoResult {
                repo: "a/timeout".into(),
                status: RepoStatus::Error(RepoError::Timeout),
            },
        ];
        let out = render_inner(&results, &Theme::default_theme(), false, 80);
        assert!(out.contains("2 projects attempted, 0 with pending tasks, 1 failed"));
        assert!(out.contains("timeout after 30s"));
    }

    #[test]
    fn all_empty_shows_only_summary() {
        let results = vec![
            RepoResult {
                repo: "a/b".into(),
                status: RepoStatus::Items(vec![]),
            },
            RepoResult {
                repo: "c/d".into(),
                status: RepoStatus::Items(vec![]),
            },
        ];
        let out = render_inner(&results, &Theme::default_theme(), false, 80);
        assert_eq!(out, "2 projects checked, 0 with pending tasks\n");
    }

    #[test]
    fn by_name_returns_some_for_known_themes() {
        assert!(Theme::by_name("default").is_some());
        assert!(Theme::by_name("evangelion").is_some());
        assert!(Theme::by_name("nerv").is_some());
    }

    #[test]
    fn by_name_returns_none_for_unknown_theme() {
        assert!(Theme::by_name("matrix").is_none());
        assert!(Theme::by_name("").is_none());
    }

    #[test]
    fn nerv_theme_with_color_produces_ansi_escapes() {
        let results = vec![RepoResult {
            repo: "owner/repo".into(),
            status: RepoStatus::Items(vec![make_item(ItemKind::Issue, 1, "x", 0)]),
        }];
        let out = render_inner(&results, &Theme::nerv(), true, 80);
        assert!(out.contains("\x1b["));
    }

    #[test]
    fn nerv_theme_without_color_has_no_ansi_escapes() {
        let results = vec![RepoResult {
            repo: "owner/repo".into(),
            status: RepoStatus::Items(vec![make_item(ItemKind::Issue, 1, "x", 0)]),
        }];
        let out = render_inner(&results, &Theme::nerv(), false, 80);
        assert!(!out.contains("\x1b["));
    }

    #[test]
    fn evangelion_theme_with_color_produces_ansi_escapes() {
        let results = vec![RepoResult {
            repo: "owner/repo".into(),
            status: RepoStatus::Items(vec![make_item(ItemKind::Issue, 1, "x", 0)]),
        }];
        let out = render_inner(&results, &Theme::evangelion(), true, 80);
        assert!(out.contains("\x1b["));
    }
}
