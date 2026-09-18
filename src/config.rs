use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    /// Tracked fork name -> upstream name, auto-managed: the GraphQL path
    /// refreshes it each run, the REST path populates it on first sight of a
    /// repo. Edit or delete entries to force re-detection. A value of `""`
    /// means "checked, confirmed not a fork" rather than "unknown" (a
    /// missing entry), so `skip_serializing_if` is deliberately omitted — an
    /// all-non-fork map is still meaningful and must round-trip.
    #[serde(default)]
    pub forks: HashMap<String, String>,
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

/// Comma-separated `owner/repo` list from `$GHPENDING_REPOS`, overriding
/// the tracked list for this run only. Never written back to config.toml —
/// `add`/`rm`/`list` are unaffected, it only reshapes what the digest
/// fetches. Entries are trimmed; blanks (from stray commas or surrounding
/// whitespace) are dropped. An unset or entirely-blank var yields `None`,
/// leaving `cfg.repos` untouched.
pub fn repos_override_from_env() -> Option<Vec<String>> {
    let raw = std::env::var("GHPENDING_REPOS").ok()?;
    let repos: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if repos.is_empty() { None } else { Some(repos) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // GHPENDING_REPOS tests mutate a process-global env var, so they must
    // never run concurrently with each other (cargo test runs #[test]s in
    // parallel threads by default) or they'll read back one another's value.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn repos_override_from_env_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("GHPENDING_REPOS") };
        assert_eq!(repos_override_from_env(), None);
    }

    #[test]
    fn repos_override_from_env_parses_csv() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("GHPENDING_REPOS", "a/b,c/d") };
        assert_eq!(
            repos_override_from_env(),
            Some(vec!["a/b".to_owned(), "c/d".to_owned()])
        );
        unsafe { std::env::remove_var("GHPENDING_REPOS") };
    }

    #[test]
    fn repos_override_from_env_trims_and_drops_blanks() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("GHPENDING_REPOS", " a/b , , c/d ,") };
        assert_eq!(
            repos_override_from_env(),
            Some(vec!["a/b".to_owned(), "c/d".to_owned()])
        );
        unsafe { std::env::remove_var("GHPENDING_REPOS") };
    }

    #[test]
    fn repos_override_from_env_all_blank_is_none() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("GHPENDING_REPOS", " , , ") };
        assert_eq!(repos_override_from_env(), None);
        unsafe { std::env::remove_var("GHPENDING_REPOS") };
    }

    #[test]
    fn round_trip_with_user() {
        let cfg = Config {
            user: Some("octocat".into()),
            repos: vec!["owner/repo".into(), "foo/bar".into()],
            theme: None,
            sort: None,
            forks: HashMap::new(),
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
            forks: HashMap::new(),
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
        assert!(cfg.forks.is_empty());
    }

    #[test]
    fn round_trip_with_theme() {
        let cfg = Config {
            user: Some("octocat".into()),
            repos: vec!["owner/repo".into()],
            theme: Some("nerv".into()),
            sort: None,
            forks: HashMap::new(),
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
            forks: HashMap::new(),
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
            forks: HashMap::new(),
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
            forks: HashMap::new(),
        };
        let s = toml::to_string(&cfg).unwrap();
        assert!(!s.contains("sort"));
        let back: Config = toml::from_str(&s).unwrap();
        assert!(back.sort.is_none());
    }

    #[test]
    fn round_trip_with_forks() {
        let mut forks = HashMap::new();
        forks.insert(
            "akitaonrails/omarchy".to_owned(),
            "omacom/omarchy".to_owned(),
        );
        forks.insert("acme/normal".to_owned(), String::new());
        let cfg = Config {
            user: Some("octocat".into()),
            repos: vec!["akitaonrails/omarchy".into(), "acme/normal".into()],
            theme: None,
            sort: None,
            forks,
        };
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(
            back.forks.get("akitaonrails/omarchy").map(String::as_str),
            Some("omacom/omarchy")
        );
        // "" (known non-fork) must round-trip too, not be dropped.
        assert_eq!(back.forks.get("acme/normal").map(String::as_str), Some(""));
    }

    #[test]
    fn round_trip_forks_empty_map() {
        let cfg = Config {
            user: None,
            repos: vec![],
            theme: None,
            sort: None,
            forks: HashMap::new(),
        };
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert!(back.forks.is_empty());
    }
}
