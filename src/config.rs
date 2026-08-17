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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
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
            sort: None,
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
            sort: None,
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
        assert!(cfg.sort.is_none());
    }

    #[test]
    fn round_trip_with_theme() {
        let cfg = Config {
            user: Some("octocat".into()),
            repos: vec!["owner/repo".into()],
            theme: Some("nerv".into()),
            sort: None,
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
            sort: None,
        };
        let s = toml::to_string(&cfg).unwrap();
        assert!(!s.contains("theme"));
        let back: Config = toml::from_str(&s).unwrap();
        assert!(back.theme.is_none());
    }

    #[test]
    fn round_trip_with_sort() {
        let cfg = Config {
            user: Some("octocat".into()),
            repos: vec!["owner/repo".into()],
            theme: None,
            sort: Some("name".into()),
        };
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.sort.as_deref(), Some("name"));
    }

    #[test]
    fn round_trip_sort_none_omitted() {
        let cfg = Config {
            user: None,
            repos: vec![],
            theme: None,
            sort: None,
        };
        let s = toml::to_string(&cfg).unwrap();
        assert!(!s.contains("sort"));
        let back: Config = toml::from_str(&s).unwrap();
        assert!(back.sort.is_none());
    }
}
