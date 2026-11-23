//! Lightweight HTTPS client backed by `ureq` for OpenRouter metadata, key
//! verification, and vocabulary capability probes. Redirects are disabled so
//! credentials are never forwarded across origins, and responses are bounded.

use std::io::Read;
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::{bail, eyre};

#[derive(Debug)]
pub(crate) struct Response {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Cap outgoing probe/config payloads to match the legacy request ceiling.
pub(crate) const MAX_BODY_BYTES: usize = 9 * 1024 * 1024;

/// Ceiling for provider-supplied Retry-After values; the caller applies its
/// own smaller cap, this only keeps the parse from panicking on huge inputs.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) fn post_json(
    url: &str,
    api_key: &str,
    body: &str,
    timeout: Duration,
) -> Result<Response> {
    if body.len() > MAX_BODY_BYTES {
        bail!("request body is too large ({} bytes)", body.len());
    }
    request(url, api_key, Some(body), timeout)
}

pub(crate) fn get(url: &str, api_key: &str, timeout: Duration) -> Result<Response> {
    request(url, api_key, None, timeout)
}

/// Apply the same origin policy before resolving credentials in every transport.
/// Parse the host rather than trusting prefixes such as `localhost.evil`.
pub(crate) fn validate_url(value: &str) -> Result<()> {
    if value
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '\\')
    {
        bail!("The provider URL contains invalid characters.");
    }
    let url = url::Url::parse(value).map_err(|_| eyre!("The provider URL is invalid."))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        bail!("Provider URLs cannot contain credentials or fragments.");
    }
    let host = url
        .host()
        .ok_or_else(|| eyre!("The provider URL needs a host."))?;
    let loopback = match host {
        url::Host::Domain(host) => host.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("Provider URLs require HTTPS; HTTP is allowed only on loopback addresses.");
    }
    Ok(())
}

fn request(url: &str, api_key: &str, body: Option<&str>, timeout: Duration) -> Result<Response> {
    validate_url(url)?;
    if api_key.chars().any(char::is_control) {
        bail!("unsupported control character in request");
    }
    let timeout = timeout.max(Duration::from_secs(1));
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(10).min(timeout)))
            .timeout_global(Some(timeout))
            .build(),
    );
    let mut response = if let Some(body) = body {
        let mut req = agent
            .post(url)
            .header("X-Title", "Endu")
            .header("Content-Type", "application/json");
        if !api_key.is_empty() {
            req = req.header("Authorization", &format!("Bearer {api_key}"));
        }
        req.send(body)
            .map_err(|error| eyre!("network error: {error}"))?
    } else {
        let mut req = agent.get(url).header("X-Title", "Endu");
        if !api_key.is_empty() {
            req = req.header("Authorization", &format!("Bearer {api_key}"));
        }
        req.call()
            .map_err(|error| eyre!("network error: {error}"))?
    };
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after);
    // Provider-controlled responses are read with a hard cap so a hostile
    // endpoint cannot balloon Endu's memory.
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take((MAX_BODY_BYTES * 2) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| eyre!("could not read response body: {error}"))?;
    Ok(Response {
        status,
        retry_after,
        body: bytes,
    })
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    let seconds = value.trim().parse::<f64>().ok()?;
    // Provider-controlled values must never panic the worker: cap at the
    // largest representable wait and let the caller's own cap decide.
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    if seconds >= MAX_RETRY_AFTER.as_secs_f64() {
        Some(MAX_RETRY_AFTER)
    } else {
        Some(Duration::from_secs_f64(seconds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_urls_require_tls_except_true_loopback_hosts() {
        for url in [
            "https://api.example.test/v1",
            "http://localhost:8080/v1",
            "http://127.0.0.1/v1",
            "http://[::1]:8080/v1",
        ] {
            assert!(validate_url(url).is_ok(), "{url}");
        }
        for url in [
            "http://example.test/v1",
            "http://localhost.evil.test",
            "http://127.0.0.1.evil.test",
            "http://localhost@evil.test",
            "http://127.0.0.1\\@evil.test",
            "http://[::1].evil.test",
            "https://user:secret@example.test",
            "file:///tmp/key",
            "https://example.test/#fragment",
            " https://example.test",
            "https://example.test/\n",
        ] {
            assert!(validate_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn parses_numeric_retry_after_and_ignores_http_dates() {
        assert_eq!(parse_retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(
            parse_retry_after(" 1.5 "),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("-1"), None);
        assert_eq!(parse_retry_after("1e300"), Some(MAX_RETRY_AFTER));
    }

    #[test]
    fn rejects_control_characters_in_api_key() {
        assert!(
            request(
                "http://127.0.0.1:1/v1",
                "sk-\n",
                None,
                Duration::from_secs(1)
            )
            .is_err()
        );
    }
}
