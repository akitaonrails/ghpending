//! Minimal HTTPS client for the GitLab API v4.
//!
//! Unlike the GitHub client this one is always **direct**: a self-hosted GitLab
//! is typically reachable only on an internal network, so routing it through the
//! SOCKS/Tor proxy would break it.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use http::{Request, StatusCode, Uri, header::ACCEPT, header::USER_AGENT};
use http_body_util::{BodyExt, Empty};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde::de::DeserializeOwned;

use crate::config::GitlabConfig;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Response body plus the pagination cursor GitLab returns in `x-next-page`.
pub struct Page<T> {
    pub items: T,
    pub next_page: Option<String>,
}

pub struct GitlabClient {
    http: Client<hyper_rustls::HttpsConnector<HttpConnector>, Empty<&'static [u8]>>,
    /// API root, e.g. `https://gitlab.com/api/v4` — no trailing slash.
    base: String,
    token: Option<String>,
    /// Host of the configured instance, used to label results in the digest.
    host: String,
}

/// Host of a GitLab instance URL, e.g. `https://gitlab.dpe.br/` → `gitlab.dpe.br`.
/// Split out from [`build`] so `list`/`rm` can label projects without paying for
/// a client they never use.
pub fn host_from_url(url: &str) -> Result<String> {
    let uri: Uri = url
        .parse()
        .with_context(|| format!("gitlab url is not a valid URI: {url}"))?;
    match uri.scheme_str() {
        Some("http") | Some("https") => {}
        _ => bail!("gitlab url must start with http:// or https:// (got {url})"),
    }
    Ok(uri
        .host()
        .with_context(|| format!("gitlab url has no host: {url}"))?
        .to_owned())
}

/// Builds a client for a configured `[gitlab]` section.
pub fn build(cfg: &GitlabConfig) -> Result<GitlabClient> {
    let url = cfg.effective_url();
    let host = host_from_url(url)?;

    let mut connector = HttpConnector::new();
    connector.enforce_http(false);
    connector.set_connect_timeout(Some(CONNECT_TIMEOUT));

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .context("loading native root certificates for GitLab")?
        .https_or_http()
        .enable_http1()
        .wrap_connector(connector);

    let http = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(READ_TIMEOUT)
        .build(https);

    Ok(GitlabClient {
        http,
        base: format!("{}/api/v4", url.trim_end_matches('/')),
        token: gitlab_token(cfg),
        host,
    })
}

/// `GITLAB_TOKEN` wins over the config value so a shell can override the file.
fn gitlab_token(cfg: &GitlabConfig) -> Option<String> {
    std::env::var("GITLAB_TOKEN")
        .ok()
        .or_else(|| cfg.token.clone())
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
}

impl GitlabClient {
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Whether any token was resolved. Anonymous `membership=true` requests come
    /// back as an empty list rather than 401, so callers need this to tell
    /// "you're in no projects" apart from "you never authenticated".
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// GETs `path` (already URL-encoded) with `query`, deserializing the JSON
    /// body into `T`. `Err` carries the HTTP status when the server answered.
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &str,
    ) -> std::result::Result<Page<T>, GitlabHttpError> {
        let url = format!("{}{path}?{query}", self.base);
        let mut builder = Request::get(&url)
            .header(USER_AGENT, "ghpending")
            .header(ACCEPT, "application/json");
        if let Some(token) = &self.token {
            builder = builder.header("PRIVATE-TOKEN", token.as_str());
        }
        let request = builder
            .body(Empty::new())
            .map_err(|e| GitlabHttpError::Other(anyhow!(e).context("building GitLab request")))?;

        let response = tokio::time::timeout(READ_TIMEOUT, self.http.request(request))
            .await
            .map_err(|_| GitlabHttpError::Other(anyhow!("GitLab request timed out: {url}")))?
            .map_err(|e| GitlabHttpError::Other(anyhow!(e).context(format!("requesting {url}"))))?;

        let status = response.status();
        let (parts, body) = response.into_parts();

        if !status.is_success() {
            return Err(GitlabHttpError::Status(status));
        }

        // `x-next-page` is empty on the last page.
        let next_page = parts
            .headers
            .get("x-next-page")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);

        let bytes = body
            .collect()
            .await
            .map_err(|e| GitlabHttpError::Other(anyhow!(e).context(format!("reading {url}"))))?
            .to_bytes();

        let items = serde_json::from_slice(&bytes).map_err(|e| {
            GitlabHttpError::Other(
                anyhow!(e).context(format!("parsing GitLab response from {url}")),
            )
        })?;

        Ok(Page { items, next_page })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GitlabHttpError {
    #[error("http {0}")]
    Status(StatusCode),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_from_url_extracts_host() {
        assert_eq!(host_from_url("https://gitlab.com").unwrap(), "gitlab.com");
        assert_eq!(
            host_from_url("https://gitlab.dpe.br/").unwrap(),
            "gitlab.dpe.br"
        );
        assert_eq!(
            host_from_url("http://gitlab.local:8080").unwrap(),
            "gitlab.local"
        );
    }

    #[test]
    fn host_from_url_rejects_non_http_scheme() {
        assert!(host_from_url("ftp://gitlab.example.org").is_err());
        assert!(host_from_url("gitlab.example.org").is_err());
    }

    #[test]
    fn build_rejects_non_http_url() {
        assert!(
            build(&GitlabConfig {
                url: "ftp://gitlab.example.org".into(),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn build_strips_trailing_slash_from_api_base() {
        let client = build(&GitlabConfig {
            url: "https://gitlab.example.org/".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(client.base, "https://gitlab.example.org/api/v4");
        assert_eq!(client.host(), "gitlab.example.org");
    }

    #[test]
    fn build_defaults_to_public_gitlab() {
        let client = build(&GitlabConfig::default()).unwrap();
        assert_eq!(client.base, "https://gitlab.com/api/v4");
        assert_eq!(client.host(), "gitlab.com");
    }

    #[test]
    fn config_token_is_used_when_env_is_unset() {
        // Only meaningful when the ambient env has no GITLAB_TOKEN.
        if std::env::var("GITLAB_TOKEN").is_ok() {
            return;
        }
        let cfg = GitlabConfig {
            token: Some("  from-config  ".into()),
            ..Default::default()
        };
        assert_eq!(gitlab_token(&cfg).as_deref(), Some("from-config"));
    }

    #[test]
    fn blank_token_is_treated_as_absent() {
        if std::env::var("GITLAB_TOKEN").is_ok() {
            return;
        }
        let cfg = GitlabConfig {
            token: Some("   ".into()),
            ..Default::default()
        };
        assert!(gitlab_token(&cfg).is_none());
    }
}
