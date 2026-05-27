use anyhow::{Result, bail};
use inquire::{MultiSelect, Text};
use octocrab::Octocrab;

use crate::{config, github};

pub async fn run(crab: &Octocrab, user: Option<String>) -> Result<()> {
    let mut cfg = config::load()?;

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

    let found = github::list_user_repos(crab, &username).await?;
    if found.is_empty() {
        println!("No public repos found for: {username}");
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
}
