use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use http::{HeaderValue, Uri, header::AUTHORIZATION, header::USER_AGENT};
use hyper_socks2::SocksConnector;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use octocrab::service::middleware::base_uri::BaseUriLayer;
use octocrab::service::middleware::extra_headers::ExtraHeadersLayer;
use octocrab::{AuthState, OctoBody, Octocrab, OctocrabBuilder};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const PROXY_CHECK_TIMEOUT: Duration = Duration::from_millis(250);

const GITHUB_BASE_URI: &str = "https://api.github.com";
const DEFAULT_GITHUB_PROXY_ADDR: &str = "127.0.0.1:9050";
const DEFAULT_GITHUB_PROXY_URI: &str = "socks5://127.0.0.1:9050";

pub fn build() -> Result<Octocrab> {
    let token = github_token();

    if let Some(proxy) = proxy_from_environment_or_local_port()? {
        match build_proxied(token.as_deref(), proxy.uri) {
            Ok(crab) => return Ok(crab),
            Err(err) if proxy.strict => return Err(err),
            Err(_) => {}
        }
    }

    build_direct(token)
}

fn build_direct(token: Option<String>) -> Result<Octocrab> {
    let mut builder = OctocrabBuilder::default()
        .set_connect_timeout(Some(web_time::Duration::from_secs(
            CONNECT_TIMEOUT.as_secs(),
        )))
        .set_read_timeout(Some(web_time::Duration::from_secs(READ_TIMEOUT.as_secs())))
        .set_write_timeout(Some(web_time::Duration::from_secs(WRITE_TIMEOUT.as_secs())));
    if let Some(token) = token {
        builder = builder.personal_token(token);
    }
    Ok(builder.build()?)
}

fn build_proxied(token: Option<&str>, proxy_uri: Uri) -> Result<Octocrab> {
    let mut connector = HttpConnector::new();
    connector.enforce_http(false);

    let socks = SocksConnector {
        proxy_addr: proxy_uri,
        auth: None,
        connector,
    };
    let https = socks.with_tls().context("creating SOCKS TLS connector")?;

    let timeout_builder = OctocrabBuilder::default()
        .set_connect_timeout(Some(web_time::Duration::from_secs(
            CONNECT_TIMEOUT.as_secs(),
        )))
        .set_read_timeout(Some(web_time::Duration::from_secs(READ_TIMEOUT.as_secs())))
        .set_write_timeout(Some(web_time::Duration::from_secs(WRITE_TIMEOUT.as_secs())));
    let connector = timeout_builder.set_connect_timeout_service(https);
    let client = Client::builder(TokioExecutor::new()).build::<_, OctoBody>(connector);

    let mut headers = vec![(USER_AGENT, HeaderValue::from_static("ghpending"))];
    if let Some(token) = token {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("building GitHub authorization header")?;
        value.set_sensitive(true);
        headers.push((AUTHORIZATION, value));
    }

    Ok(OctocrabBuilder::new_empty()
        .with_service(client)
        .with_layer(&BaseUriLayer::new(Uri::from_static(GITHUB_BASE_URI)))
        .with_layer(&ExtraHeadersLayer::new(Arc::new(headers)))
        .with_auth(AuthState::None)
        .build()?)
}

pub(crate) fn github_token() -> Option<String> {
    std::env::var("GITHUB_TOKEN")
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
}

#[derive(Debug)]
struct ProxyChoice {
    uri: Uri,
    strict: bool,
}

fn proxy_from_environment_or_local_port() -> Result<Option<ProxyChoice>> {
    if let Ok(raw) = std::env::var("GHPENDING_GITHUB_PROXY") {
        return Ok(Some(ProxyChoice {
            uri: normalize_required_socks_proxy_uri("GHPENDING_GITHUB_PROXY", raw.trim())?,
            strict: true,
        }));
    }

    for key in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        if let Some(uri) = proxy_uri_from_optional_env_value(key, std::env::var(key).ok())? {
            return Ok(Some(ProxyChoice { uri, strict: true }));
        }
    }

    if local_proxy_is_listening() {
        return Ok(Some(ProxyChoice {
            uri: Uri::from_static(DEFAULT_GITHUB_PROXY_URI),
            strict: false,
        }));
    }

    Ok(None)
}

fn proxy_uri_from_optional_env_value(key: &str, raw: Option<String>) -> Result<Option<Uri>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }

    let Ok(uri) = raw.parse::<Uri>() else {
        return Ok(None);
    };
    match uri.scheme_str() {
        Some("socks5") | Some("socks5h") => Ok(Some(normalize_socks_proxy_uri(key, uri)?)),
        _ => Ok(None),
    }
}

fn normalize_required_socks_proxy_uri(key: &str, raw: &str) -> Result<Uri> {
    let uri: Uri = raw
        .parse()
        .with_context(|| format!("{key} must be a valid SOCKS proxy URI"))?;
    match uri.scheme_str() {
        Some("socks5") | Some("socks5h") => normalize_socks_proxy_uri(key, uri),
        _ => bail!("{key} must use socks5:// or socks5h://"),
    }
}

fn normalize_socks_proxy_uri(key: &str, uri: Uri) -> Result<Uri> {
    let authority = uri
        .authority()
        .with_context(|| format!("{key} must include a proxy host"))?
        .as_str();
    if authority.contains('@') {
        bail!("{key} SOCKS proxy authentication is not supported");
    }
    Ok(format!("socks5://{authority}").parse()?)
}

fn local_proxy_is_listening() -> bool {
    let Ok(addr) = DEFAULT_GITHUB_PROXY_ADDR.parse::<SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, PROXY_CHECK_TIMEOUT).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_socks5h_proxy_uri_for_connector() {
        let uri =
            normalize_required_socks_proxy_uri("TEST_PROXY", "socks5h://127.0.0.1:9050").unwrap();
        assert_eq!(uri.scheme_str(), Some("socks5"));
        assert_eq!(uri.authority().map(|a| a.as_str()), Some("127.0.0.1:9050"));
    }

    #[test]
    fn accepts_socks5_proxy_uri() {
        let uri =
            normalize_required_socks_proxy_uri("TEST_PROXY", "socks5://localhost:1080").unwrap();
        assert_eq!(uri.scheme_str(), Some("socks5"));
        assert_eq!(uri.authority().map(|a| a.as_str()), Some("localhost:1080"));
    }

    #[test]
    fn optional_proxy_env_ignores_http_proxy_uri() {
        assert!(
            proxy_uri_from_optional_env_value("HTTPS_PROXY", Some("http://127.0.0.1:8080".into()))
                .unwrap()
                .is_none()
        );
        assert!(
            proxy_uri_from_optional_env_value("HTTPS_PROXY", Some("https://127.0.0.1:8080".into()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn optional_proxy_env_ignores_empty_or_invalid_proxy_uri() {
        assert!(
            proxy_uri_from_optional_env_value("HTTPS_PROXY", Some("".into()))
                .unwrap()
                .is_none()
        );
        assert!(
            proxy_uri_from_optional_env_value("HTTPS_PROXY", Some("not a uri".into()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn required_proxy_rejects_http_proxy_uri() {
        assert!(normalize_required_socks_proxy_uri("TEST_PROXY", "http://127.0.0.1:8080").is_err());
    }

    #[test]
    fn rejects_socks_proxy_with_credentials() {
        assert!(
            normalize_required_socks_proxy_uri("TEST_PROXY", "socks5h://user:pass@127.0.0.1:9050")
                .is_err()
        );
    }
}
