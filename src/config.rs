use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub user: Option<String>,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Present only when the user tracks GitLab projects — absent means the
    /// GitLab client is never built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gitlab: Option<GitlabConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitlabConfig {
    /// Empty → the public https://gitlab.com. Set it to point at a self-hosted
    /// instance instead.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Prefer the `GITLAB_TOKEN` env var; this is the fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default)]
    pub projects: Vec<String>,
}

pub const DEFAULT_GITLAB_URL: &str = "https://gitlab.com";

impl GitlabConfig {
    /// The configured URL, or the public gitlab.com when none was given.
    pub fn effective_url(&self) -> &str {
        let url = self.url.trim();
        if url.is_empty() {
            DEFAULT_GITLAB_URL
        } else {
            url
        }
    }
}

fn config_path() -> Result<PathBuf> {
    let proj =
        ProjectDirs::from("", "", "ghpending").context("could not determine config directory")?;
    Ok(proj.config_dir().join("config.toml"))
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating dir {}", parent.display()))?;
    }
    let text = toml::to_string(cfg).context("serializing config")?;
    std::fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_user() {
        let cfg = Config {
            user: Some("octocat".into()),
            repos: vec!["owner/repo".into(), "foo/bar".into()],
            theme: None,
            gitlab: None,
        };
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.user.as_deref(), Some("octocat"));
        assert_eq!(back.repos, vec!["owner/repo", "foo/bar"]);
    }

    #[test]
    fn round_trip_user_none() {
        let cfg = Config {
            user: None,
            repos: vec!["owner/repo".into()],
            theme: None,
            gitlab: None,
        };
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert!(back.user.is_none());
        assert_eq!(back.repos, vec!["owner/repo"]);
    }

    #[test]
    fn default_on_missing_file() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.user.is_none());
        assert!(cfg.repos.is_empty());
        assert!(cfg.theme.is_none());
        assert!(cfg.gitlab.is_none());
    }

    #[test]
    fn round_trip_with_theme() {
        let cfg = Config {
            user: Some("octocat".into()),
            repos: vec!["owner/repo".into()],
            theme: Some("nerv".into()),
            gitlab: None,
        };
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.theme.as_deref(), Some("nerv"));
    }

    #[test]
    fn round_trip_theme_none_omitted() {
        let cfg = Config {
            user: None,
            repos: vec![],
            theme: None,
            gitlab: None,
        };
        let s = toml::to_string(&cfg).unwrap();
        assert!(!s.contains("theme"));
        let back: Config = toml::from_str(&s).unwrap();
        assert!(back.theme.is_none());
    }

    #[test]
    fn round_trip_with_gitlab_section() {
        let cfg = Config {
            user: None,
            repos: vec!["owner/repo".into()],
            theme: None,
            gitlab: Some(GitlabConfig {
                url: "https://gitlab.example.org".into(),
                token: Some("secret".into()),
                projects: vec!["group/sub/app".into()],
            }),
        };
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        let gl = back.gitlab.unwrap();
        assert_eq!(gl.effective_url(), "https://gitlab.example.org");
        assert_eq!(gl.token.as_deref(), Some("secret"));
        assert_eq!(gl.projects, vec!["group/sub/app"]);
    }

    #[test]
    fn gitlab_without_url_defaults_to_public_gitlab() {
        let cfg: Config = toml::from_str(
            r#"
repos = []
[gitlab]
projects = ["gitlab-org/gitlab-runner"]
"#,
        )
        .unwrap();
        let gl = cfg.gitlab.unwrap();
        assert_eq!(gl.effective_url(), "https://gitlab.com");
        assert!(gl.token.is_none());
    }

    #[test]
    fn gitlab_blank_url_defaults_to_public_gitlab() {
        let gl = GitlabConfig {
            url: "   ".into(),
            ..Default::default()
        };
        assert_eq!(gl.effective_url(), "https://gitlab.com");
    }

    #[test]
    fn gitlab_url_is_trimmed() {
        let gl = GitlabConfig {
            url: "  https://gitlab.dpe.br  ".into(),
            ..Default::default()
        };
        assert_eq!(gl.effective_url(), "https://gitlab.dpe.br");
    }

    #[test]
    fn gitlab_none_is_omitted_from_serialization() {
        let cfg = Config::default();
        let s = toml::to_string(&cfg).unwrap();
        assert!(!s.contains("gitlab"));
    }
}
