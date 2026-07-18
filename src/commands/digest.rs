use std::time::Duration;

use anyhow::Result;
use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use octocrab::Octocrab;
use tokio::time::{self, timeout};

use crate::gitlab_client::GitlabClient;
use crate::model::{RepoError, RepoResult, RepoStatus};
use crate::theme::Theme;
use crate::{config, display, github, gitlab};

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONCURRENT_FETCHES: usize = 4;

/// One repo/project to fetch, tagged with the provider that owns it. `label` is
/// what the digest prints; for GitLab it carries the instance host.
#[derive(Debug, Clone)]
enum FetchTask {
    Github { repo: String },
    Gitlab { project: String, label: String },
}

impl FetchTask {
    fn label(&self) -> &str {
        match self {
            FetchTask::Github { repo } => repo,
            FetchTask::Gitlab { label, .. } => label,
        }
    }
}

pub async fn run(
    crab: &Octocrab,
    gitlab_client: Option<&GitlabClient>,
    theme: &Theme,
) -> Result<()> {
    let cfg = config::load()?;

    let tasks = build_tasks(&cfg, gitlab_client);

    if tasks.is_empty() {
        println!("No repos tracked. Run `ghpending add` to get started.");
        return Ok(());
    }

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    spinner.set_message("Fetching…");
    spinner.enable_steady_tick(Duration::from_millis(100));

    let results = fetch_all(crab, gitlab_client, &tasks).await;

    spinner.finish_and_clear();

    let digest = display::render_digest(&results, theme);
    print!("{digest}");

    if all_repo_fetches_failed(&results) {
        anyhow::bail!("all repository fetches failed");
    }

    Ok(())
}

/// GitHub repos first, then GitLab projects — GitLab entries only when a client
/// was built for them.
fn build_tasks(cfg: &config::Config, gitlab_client: Option<&GitlabClient>) -> Vec<FetchTask> {
    let mut tasks: Vec<FetchTask> = cfg
        .repos
        .iter()
        .map(|repo| FetchTask::Github { repo: repo.clone() })
        .collect();

    if let (Some(client), Some(gl_cfg)) = (gitlab_client, cfg.gitlab.as_ref()) {
        tasks.extend(gl_cfg.projects.iter().map(|project| FetchTask::Gitlab {
            project: project.clone(),
            label: gitlab::project_label(client.host(), project),
        }));
    }

    tasks
}

async fn fetch_all(
    crab: &Octocrab,
    gitlab_client: Option<&GitlabClient>,
    tasks: &[FetchTask],
) -> Vec<RepoResult> {
    let mut results = vec![None; tasks.len()];
    let mut in_flight = FuturesUnordered::new();
    let mut next = 0;

    while next < tasks.len() && in_flight.len() < MAX_CONCURRENT_FETCHES {
        in_flight.push(fetch_one_with_timeout(
            crab,
            gitlab_client,
            next,
            &tasks[next],
        ));
        next += 1;
    }

    let deadline = time::sleep(FETCH_TIMEOUT);
    tokio::pin!(deadline);

    while !in_flight.is_empty() {
        tokio::select! {
            _ = &mut deadline => break,
            Some((index, result)) = in_flight.next() => {
                results[index] = Some(result);

                if next < tasks.len() {
                    in_flight.push(fetch_one_with_timeout(crab, gitlab_client, next, &tasks[next]));
                    next += 1;
                }
            }
        }
    }

    results
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            result.unwrap_or_else(|| timeout_result(tasks[index].label().to_owned()))
        })
        .collect()
}

async fn fetch_one_with_timeout(
    crab: &Octocrab,
    gitlab_client: Option<&GitlabClient>,
    index: usize,
    task: &FetchTask,
) -> (usize, RepoResult) {
    let result = match timeout(FETCH_TIMEOUT, fetch_one(crab, gitlab_client, task)).await {
        Ok(result) => result,
        Err(_) => timeout_result(task.label().to_owned()),
    };
    (index, result)
}

async fn fetch_one(
    crab: &Octocrab,
    gitlab_client: Option<&GitlabClient>,
    task: &FetchTask,
) -> RepoResult {
    match task {
        FetchTask::Github { repo } => github::fetch_repo_items(crab, repo).await,
        FetchTask::Gitlab { project, label } => match gitlab_client {
            Some(client) => gitlab::fetch_project_items(client, project, label).await,
            // Unreachable: GitLab tasks are only built when a client exists.
            None => RepoResult {
                repo: label.clone(),
                status: RepoStatus::Error(RepoError::Api("gitlab client unavailable".into())),
            },
        },
    }
}

fn timeout_result(repo: String) -> RepoResult {
    RepoResult {
        repo,
        status: RepoStatus::Error(RepoError::Timeout),
    }
}

pub(crate) fn all_repo_fetches_failed(results: &[RepoResult]) -> bool {
    !results.is_empty()
        && results
            .iter()
            .all(|result| matches!(result.status, RepoStatus::Error(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, GitlabConfig};

    fn config_with(repos: &[&str], gitlab: Option<GitlabConfig>) -> Config {
        Config {
            user: None,
            repos: repos.iter().map(|r| (*r).to_string()).collect(),
            theme: None,
            gitlab,
        }
    }

    #[test]
    fn all_repo_fetches_failed_requires_every_result_to_be_error() {
        assert!(all_repo_fetches_failed(&[
            RepoResult {
                repo: "a/b".into(),
                status: RepoStatus::Error(RepoError::Timeout),
            },
            RepoResult {
                repo: "c/d".into(),
                status: RepoStatus::Error(RepoError::Api("boom".into())),
            },
        ]));

        assert!(!all_repo_fetches_failed(&[RepoResult {
            repo: "a/b".into(),
            status: RepoStatus::NotFound,
        }]));

        assert!(!all_repo_fetches_failed(&[RepoResult {
            repo: "a/b".into(),
            status: RepoStatus::Items(vec![]),
        }]));

        assert!(!all_repo_fetches_failed(&[]));
    }

    #[test]
    fn builds_only_github_tasks_without_a_gitlab_client() {
        let cfg = config_with(&["owner/repo"], None);
        let tasks = build_tasks(&cfg, None);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].label(), "owner/repo");
    }

    #[test]
    fn skips_gitlab_projects_when_client_is_absent() {
        // A `[gitlab]` section with no client must not produce phantom tasks.
        let cfg = config_with(
            &["owner/repo"],
            Some(GitlabConfig {
                projects: vec!["group/app".into()],
                ..Default::default()
            }),
        );
        let tasks = build_tasks(&cfg, None);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].label(), "owner/repo");
    }

    #[test]
    fn builds_gitlab_tasks_labelled_with_the_instance_host() {
        let gl_cfg = GitlabConfig {
            url: "https://gitlab.dpe.br".into(),
            projects: vec!["nucleo-ti/portal".into()],
            ..Default::default()
        };
        let client = crate::gitlab_client::build(&gl_cfg).unwrap();
        let cfg = config_with(&["owner/repo"], Some(gl_cfg));

        let tasks = build_tasks(&cfg, Some(&client));

        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].label(), "owner/repo");
        assert_eq!(tasks[1].label(), "gitlab.dpe.br/nucleo-ti/portal");
        assert!(
            matches!(&tasks[1], FetchTask::Gitlab { project, .. } if project == "nucleo-ti/portal")
        );
    }
}
