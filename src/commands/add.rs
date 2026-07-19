use std::{fmt, future::Future, time::Duration};

use anyhow::{Context, Result, bail};
use inquire::{MultiSelect, Select, Text};
use octocrab::Octocrab;
use tokio::time::timeout;

use crate::config::DEFAULT_GITLAB_URL;
use crate::github::ListSource;
use crate::{config, github, gitlab, gitlab_client};

const API_TIMEOUT: Duration = Duration::from_secs(30);

/// Which forge `add` is adding to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Github,
    Gitlab,
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Provider::Github => write!(f, "GitHub"),
            Provider::Gitlab => write!(f, "GitLab"),
        }
    }
}

/// `--user` and `--all` are GitHub-only, so either one settles the provider and
/// the picker is skipped. `None` means we have to ask.
fn provider_from_flags(user: Option<&str>, all: bool) -> Option<Provider> {
    if all || user.is_some() {
        Some(Provider::Github)
    } else {
        None
    }
}

pub async fn run(crab: &Octocrab, user: Option<String>, all: bool) -> Result<()> {
    let provider = match provider_from_flags(user.as_deref(), all) {
        Some(provider) => provider,
        None => {
            Select::new("Add repos from:", vec![Provider::Github, Provider::Gitlab]).prompt()?
        }
    };

    match provider {
        Provider::Github => add_github(crab, user, all).await,
        Provider::Gitlab => add_gitlab().await,
    }
}

async fn add_github(crab: &Octocrab, user: Option<String>, all: bool) -> Result<()> {
    let mut cfg = config::load()?;

    let found = if all {
        with_api_timeout(
            github::list_authenticated_repos(crab),
            "listing repositories timed out after 30s",
        )
        .await?
    } else {
        let username = match resolve_user(user, cfg.user.clone()) {
            UserChoice::Override(u) => {
                cfg.user = Some(u.clone());
                config::save(&cfg)?;
                u
            }
            UserChoice::Saved(u) => u,
            UserChoice::Prompt => {
                let u = Text::new("GitHub username or org to list repos from:")
                    .prompt()?
                    .trim()
                    .to_owned();
                cfg.user = Some(u.clone());
                config::save(&cfg)?;
                u
            }
            UserChoice::Blank => bail!("--user cannot be empty"),
        };

        match with_api_timeout(
            github::resolve_source_for(crab, &username),
            "resolving repository source timed out after 30s",
        )
        .await?
        {
            ListSource::Authenticated => {
                with_api_timeout(
                    github::list_authenticated_repos(crab),
                    "listing repositories timed out after 30s",
                )
                .await?
            }
            ListSource::Org(org) => {
                with_api_timeout(
                    github::list_org_repos(crab, &org),
                    "listing repositories timed out after 30s",
                )
                .await?
            }
            ListSource::PublicUser(u) => {
                with_api_timeout(
                    github::list_user_repos(crab, &u),
                    "listing repositories timed out after 30s",
                )
                .await?
            }
        }
    };

    if found.is_empty() {
        if all {
            println!("No repos found for your account.");
        } else {
            println!("No repos found.");
        }
        return Ok(());
    }

    let already: std::collections::HashSet<&str> =
        cfg.repos.iter().map(std::string::String::as_str).collect();

    let defaults: Vec<usize> = found
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            if already.contains(r.as_str()) {
                Some(i)
            } else {
                None
            }
        })
        .collect();

    let selected = MultiSelect::new("Select repos to track:", found)
        .with_default(&defaults)
        .prompt()?;

    for repo in selected {
        if !cfg.repos.contains(&repo) {
            cfg.repos.push(repo);
        }
    }
    cfg.repos.sort();
    config::save(&cfg)?;
    println!("Saved. Tracking {} repo(s) total.", cfg.repos.len());
    Ok(())
}

async fn with_api_timeout<T>(
    future: impl Future<Output = Result<T>>,
    message: &'static str,
) -> Result<T> {
    timeout(API_TIMEOUT, future).await.context(message)?
}

/// Where the GitLab picker should pull projects from.
#[derive(Debug, PartialEq)]
enum GroupChoice {
    /// Blank input: everything the token is a member of.
    Membership,
    Group(String),
}

fn group_selection(input: &str) -> GroupChoice {
    let input = input.trim();
    if input.is_empty() {
        GroupChoice::Membership
    } else {
        GroupChoice::Group(input.to_owned())
    }
}

/// Unlike the GitHub path, this builds its own client: `main` only builds one
/// when `[gitlab]` already exists, and `add` is precisely how that section gets
/// created in the first place.
async fn add_gitlab() -> Result<()> {
    let mut cfg = config::load()?;
    let mut gl_cfg = cfg.gitlab.clone().unwrap_or_default();

    // Only ask for the instance the first time; afterwards it is settled.
    if cfg.gitlab.is_none() {
        let url = Text::new("GitLab instance URL:")
            .with_default(DEFAULT_GITLAB_URL)
            .prompt()?
            .trim()
            .to_owned();
        if url.is_empty() {
            bail!("GitLab URL cannot be empty");
        }
        gl_cfg.url = url;

        // Persist the instance right away. Everything after this can bail out
        // (no token, empty listing, aborted picker) and the user should not
        // have to retype the URL on the next run.
        gitlab_client::host_from_url(gl_cfg.effective_url())?;
        cfg.gitlab = Some(gl_cfg.clone());
        config::save(&cfg)?;
    }

    let client = gitlab_client::build(&gl_cfg)?;

    let group_prompt = Text::new("Group to list projects from (blank = all your projects):");
    let group_input = match gl_cfg.group.as_deref() {
        Some(saved) => group_prompt.with_default(saved).prompt()?,
        None => group_prompt.prompt()?,
    };

    let (found, group) = match group_selection(&group_input) {
        GroupChoice::Membership => (
            with_api_timeout(
                gitlab::list_membership_projects(&client),
                "listing GitLab projects timed out after 30s",
            )
            .await?,
            None,
        ),
        GroupChoice::Group(group) => (
            with_api_timeout(
                gitlab::list_group_projects(&client, &group),
                "listing GitLab projects timed out after 30s",
            )
            .await?,
            Some(group),
        ),
    };

    if found.is_empty() {
        // An anonymous membership listing succeeds with an empty body rather
        // than failing, so spell out the likely cause instead of a dead end.
        if !client.has_token() {
            println!(
                "No GitLab projects found. Set GITLAB_TOKEN to a personal access \
                 token with the `read_api` scope to see the projects you belong to."
            );
        } else {
            println!("No GitLab projects found.");
        }
        return Ok(());
    }

    let already: std::collections::HashSet<&str> = gl_cfg
        .projects
        .iter()
        .map(std::string::String::as_str)
        .collect();
    let defaults: Vec<usize> = found
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            if already.contains(p.as_str()) {
                Some(i)
            } else {
                None
            }
        })
        .collect();

    let selected = MultiSelect::new("Select projects to track:", found)
        .with_default(&defaults)
        .prompt()?;

    for project in selected {
        if !gl_cfg.projects.contains(&project) {
            gl_cfg.projects.push(project);
        }
    }
    gl_cfg.projects.sort();
    gl_cfg.group = group;

    let total = gl_cfg.projects.len();
    cfg.gitlab = Some(gl_cfg);
    config::save(&cfg)?;
    println!("Saved. Tracking {total} GitLab project(s) total.");
    Ok(())
}

/// Which GitHub user/org `add` should list repos from, decided from the
/// optional `--user` flag and whatever is already saved in config.
#[derive(Debug, PartialEq)]
enum UserChoice {
    /// `--user` was given: use it and persist it as the new saved default.
    Override(String),
    /// No flag, but config already holds a user: reuse it untouched.
    Saved(String),
    /// Neither flag nor saved user: prompt for one interactively.
    Prompt,
    /// `--user` was given but blank once trimmed.
    Blank,
}

fn resolve_user(flag: Option<String>, saved: Option<String>) -> UserChoice {
    match flag {
        Some(u) => {
            let u = u.trim();
            if u.is_empty() {
                UserChoice::Blank
            } else {
                UserChoice::Override(u.to_owned())
            }
        }
        None => match saved {
            Some(u) => UserChoice::Saved(u),
            None => UserChoice::Prompt,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_overrides_saved_user() {
        let choice = resolve_user(Some("octocat".into()), Some("akitaonrails".into()));
        assert_eq!(choice, UserChoice::Override("octocat".into()));
    }

    #[test]
    fn flag_is_trimmed() {
        let choice = resolve_user(Some("  octocat  ".into()), None);
        assert_eq!(choice, UserChoice::Override("octocat".into()));
    }

    #[test]
    fn blank_flag_is_rejected_over_saved_user() {
        let choice = resolve_user(Some("   ".into()), Some("akitaonrails".into()));
        assert_eq!(choice, UserChoice::Blank);
    }

    #[test]
    fn falls_back_to_saved_user_without_flag() {
        let choice = resolve_user(None, Some("akitaonrails".into()));
        assert_eq!(choice, UserChoice::Saved("akitaonrails".into()));
    }

    #[test]
    fn prompts_when_nothing_supplied_or_saved() {
        assert_eq!(resolve_user(None, None), UserChoice::Prompt);
    }

    #[test]
    fn github_only_flags_skip_the_provider_picker() {
        assert_eq!(
            provider_from_flags(Some("octocat"), false),
            Some(Provider::Github)
        );
        assert_eq!(provider_from_flags(None, true), Some(Provider::Github));
    }

    #[test]
    fn bare_add_asks_which_provider() {
        assert_eq!(provider_from_flags(None, false), None);
    }

    #[test]
    fn blank_group_lists_everything_you_belong_to() {
        assert_eq!(group_selection(""), GroupChoice::Membership);
        assert_eq!(group_selection("   "), GroupChoice::Membership);
    }

    #[test]
    fn group_input_is_trimmed() {
        assert_eq!(
            group_selection("  defensoria/solar  "),
            GroupChoice::Group("defensoria/solar".into())
        );
    }
}
