use anyhow::Result;
use inquire::MultiSelect;

use crate::commands::list::{Target, WatchEntry, watch_entries};
use crate::config;

pub fn run() -> Result<()> {
    let mut cfg = config::load()?;
    let entries = watch_entries(&cfg);

    if entries.is_empty() {
        println!("No repos tracked.");
        return Ok(());
    }

    let to_remove = MultiSelect::new("Select repos to remove:", entries).prompt()?;
    let removed = to_remove.len();
    let (github, gitlab) = partition_removals(&to_remove);

    cfg.repos.retain(|repo| !github.contains(&repo.as_str()));
    if let Some(gl_cfg) = cfg.gitlab.as_mut() {
        gl_cfg
            .projects
            .retain(|project| !gitlab.contains(&project.as_str()));
    }

    config::save(&cfg)?;

    if removed == 0 {
        println!("Nothing removed.");
    } else {
        println!("Removed {removed} repo(s).");
    }
    Ok(())
}

/// Splits the picked entries by provider. Reading `target` rather than parsing
/// the label back apart is what keeps a GitLab project from being mistaken for
/// a GitHub repo.
fn partition_removals(selected: &[WatchEntry]) -> (Vec<&str>, Vec<&str>) {
    let mut github = Vec::new();
    let mut gitlab = Vec::new();

    for entry in selected {
        match &entry.target {
            Target::Github(repo) => github.push(repo.as_str()),
            Target::Gitlab(project) => gitlab.push(project.as_str()),
        }
    }

    (github, gitlab)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, GitlabConfig};

    fn entry(label: &str, target: Target) -> WatchEntry {
        WatchEntry {
            label: label.into(),
            target,
        }
    }

    #[test]
    fn splits_selection_by_provider() {
        let selected = vec![
            entry("owner/repo", Target::Github("owner/repo".into())),
            entry(
                "gitlab.dpe.br/nucleo-ti/portal",
                Target::Gitlab("nucleo-ti/portal".into()),
            ),
        ];

        let (github, gitlab) = partition_removals(&selected);

        assert_eq!(github, vec!["owner/repo"]);
        // The bare path is what gets removed from config, not the label.
        assert_eq!(gitlab, vec!["nucleo-ti/portal"]);
    }

    #[test]
    fn removing_a_gitlab_project_leaves_github_repos_alone() {
        let mut cfg = Config {
            repos: vec!["owner/repo".into(), "foo/bar".into()],
            gitlab: Some(GitlabConfig {
                url: "https://gitlab.dpe.br".into(),
                projects: vec!["nucleo-ti/portal".into(), "grupo/app".into()],
                ..Default::default()
            }),
            ..Default::default()
        };

        let selected = vec![entry(
            "gitlab.dpe.br/grupo/app",
            Target::Gitlab("grupo/app".into()),
        )];
        let (github, gitlab) = partition_removals(&selected);
        cfg.repos.retain(|r| !github.contains(&r.as_str()));
        cfg.gitlab
            .as_mut()
            .unwrap()
            .projects
            .retain(|p| !gitlab.contains(&p.as_str()));

        assert_eq!(cfg.repos, vec!["owner/repo", "foo/bar"]);
        assert_eq!(cfg.gitlab.unwrap().projects, vec!["nucleo-ti/portal"]);
    }

    #[test]
    fn empty_selection_removes_nothing() {
        let (github, gitlab) = partition_removals(&[]);
        assert!(github.is_empty());
        assert!(gitlab.is_empty());
    }
}
