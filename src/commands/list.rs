use anyhow::Result;

use crate::config::{self, Config};
use crate::{gitlab, gitlab_client};

/// What a tracked entry points at. The display label alone is not enough to
/// identify a provider, so `rm` carries this alongside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Github(String),
    /// The bare project path, without the host prefix the label carries.
    Gitlab(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEntry {
    pub label: String,
    pub target: Target,
}

impl std::fmt::Display for WatchEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label)
    }
}

/// Every tracked entry, GitHub first then GitLab, labeled the same way the
/// digest labels them. Shared with `rm` so the two never drift apart.
pub fn watch_entries(cfg: &Config) -> Vec<WatchEntry> {
    let mut entries: Vec<WatchEntry> = cfg
        .repos
        .iter()
        .map(|repo| WatchEntry {
            label: repo.clone(),
            target: Target::Github(repo.clone()),
        })
        .collect();

    if let Some(gl_cfg) = &cfg.gitlab {
        // A malformed URL should not stop `list`/`rm` from working: fall back to
        // the bare project path rather than erroring out.
        let host = gitlab_client::host_from_url(gl_cfg.effective_url()).ok();
        entries.extend(gl_cfg.projects.iter().map(|project| WatchEntry {
            label: match &host {
                Some(host) => gitlab::project_label(host, project),
                None => project.clone(),
            },
            target: Target::Gitlab(project.clone()),
        }));
    }

    entries
}

pub fn run() -> Result<()> {
    let cfg = config::load()?;
    let entries = watch_entries(&cfg);

    if entries.is_empty() {
        println!("No repos tracked. Run `ghpending add` to get started.");
    } else {
        for entry in &entries {
            println!("{entry}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GitlabConfig;

    #[test]
    fn lists_github_repos_first_then_gitlab_projects() {
        let cfg = Config {
            repos: vec!["owner/repo".into()],
            gitlab: Some(GitlabConfig {
                url: "https://gitlab.dpe.br".into(),
                projects: vec!["nucleo-ti/portal".into()],
                ..Default::default()
            }),
            ..Default::default()
        };

        let entries = watch_entries(&cfg);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label, "owner/repo");
        assert_eq!(entries[0].target, Target::Github("owner/repo".into()));
        assert_eq!(entries[1].label, "gitlab.dpe.br/nucleo-ti/portal");
        // The target keeps the bare path, not the labeled form.
        assert_eq!(entries[1].target, Target::Gitlab("nucleo-ti/portal".into()));
    }

    #[test]
    fn gitlab_projects_default_to_the_public_host_label() {
        let cfg = Config {
            gitlab: Some(GitlabConfig {
                projects: vec!["group/app".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(watch_entries(&cfg)[0].label, "gitlab.com/group/app");
    }

    #[test]
    fn malformed_gitlab_url_falls_back_to_the_bare_path() {
        let cfg = Config {
            gitlab: Some(GitlabConfig {
                url: "not a url".into(),
                projects: vec!["group/app".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(watch_entries(&cfg)[0].label, "group/app");
    }

    #[test]
    fn no_tracked_entries_when_config_is_empty() {
        assert!(watch_entries(&Config::default()).is_empty());
    }
}
