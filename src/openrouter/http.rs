//! One HTTPS request through the system curl, matching upstream's OpenCode client:
//! the URL, headers, credentials, and body travel on stdin as a curl config, so
//! nothing sensitive reaches argv or a temporary file.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail, eyre};

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

/// curl's config parser accepts lines up to 10 MiB; stay below it.
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
    let input = curl_config(url, api_key, body)?;
    let seconds = timeout.as_secs().max(1).to_string();
    let curl = if cfg!(target_os = "macos") {
        "/usr/bin/curl"
    } else {
        "curl"
    };
    // The bearer key and base64 audio travel in these requests; only loopback
    // endpoints (local LLM gateways) may use cleartext HTTP.
    let cleartext = url.starts_with("http://");
    let protocol = if cleartext { "=http,https" } else { "=https" };
    let mut child = Command::new(curl)
        .args([
            "--disable",
            "--silent",
            "--show-error",
            "--proto",
            protocol,
            "--proto-redir",
            protocol,
            "--connect-timeout",
            "10",
            "--max-time",
            &seconds,
            "--write-out",
            "\n%{http_code} %header{retry-after}",
            "--config",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err("could not start curl")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| eyre!("curl stdin unavailable"))?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    // Provider-controlled responses are read with a hard cap so a hostile
    // endpoint cannot balloon Hex's memory through the buffered streams.
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("curl stdout unavailable"))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| eyre!("curl stderr unavailable"))?;
    let stdout_reader = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut buffer = Vec::new();
        let pipe = stdout_pipe;
        pipe.take((MAX_BODY_BYTES * 2) as u64)
            .read_to_end(&mut buffer)?;
        Ok(buffer)
    });
    let stderr_reader = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut buffer = Vec::new();
        let pipe = stderr_pipe;
        pipe.take(64 * 1024).read_to_end(&mut buffer)?;
        Ok(buffer)
    });
    let status = child.wait().wrap_err("curl did not finish")?;
    let _ = writer.join();
    let stdout = stdout_reader
        .join()
        .map_err(|_| eyre!("curl stdout reader panicked"))?
        .wrap_err("could not read curl output")?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| eyre!("curl stderr reader panicked"))?
        .unwrap_or_default();
    let output = std::process::Output {
        status,
        stdout,
        stderr,
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("network error ({}): {}", output.status, stderr.trim());
    }
    parse_output(output.stdout).ok_or_else(|| eyre!("curl returned no HTTP status"))
}

fn curl_config(url: &str, api_key: &str, body: Option<&str>) -> Result<Vec<u8>> {
    let authorization = format!("Authorization: Bearer {api_key}");
    let mut options = vec![("url", url)];
    // Public endpoints such as the model catalog work without a key.
    if !api_key.is_empty() {
        options.push(("header", authorization.as_str()));
    }
    options.push(("header", "X-Title: Hex"));
    if let Some(body) = body {
        options.push(("request", "POST"));
        options.push(("header", "Content-Type: application/json"));
        options.push(("data-binary", body));
    }
    let mut input = String::with_capacity(body.map_or(0, str::len) + 512);
    for (key, value) in options {
        input.push_str(key);
        input.push_str(" = \"");
        for ch in value.chars() {
            match ch {
                '\\' => input.push_str("\\\\"),
                '"' => input.push_str("\\\""),
                '\n' => input.push_str("\\n"),
                '\r' => input.push_str("\\r"),
                '\t' => input.push_str("\\t"),
                ch if ch.is_control() => bail!("unsupported control character in request"),
                ch => input.push(ch),
            }
        }
        input.push_str("\"\n");
    }
    Ok(input.into_bytes())
}

/// curl appends `\n<status> <retry-after>` after the body.
fn parse_output(mut stdout: Vec<u8>) -> Option<Response> {
    let newline = stdout.iter().rposition(|byte| *byte == b'\n')?;
    let trailer = std::str::from_utf8(&stdout[newline + 1..]).ok()?.to_owned();
    stdout.truncate(newline);
    let mut parts = trailer.split_whitespace();
    let status = parts.next()?.parse().ok()?;
    let retry_after = parts
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        // Provider-controlled values must never panic the worker: cap at the
        // largest representable wait and let the caller's own cap decide.
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| {
            if seconds >= MAX_RETRY_AFTER.as_secs_f64() {
                MAX_RETRY_AFTER
            } else {
                Duration::from_secs_f64(seconds)
            }
        });
    Some(Response {
        status,
        retry_after,
        body: stdout,
    })
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
    fn parses_status_and_retry_after_trailer() {
        let response = parse_output(b"{\"text\":\"oi\"}\n429 2".to_vec()).unwrap();
        assert_eq!(response.status, 429);
        assert_eq!(response.retry_after, Some(Duration::from_secs(2)));
        assert_eq!(response.body, b"{\"text\":\"oi\"}");
    }

    #[test]
    fn missing_retry_after_is_none_and_body_newlines_survive() {
        let response = parse_output(b"a\nb\n200 ".to_vec()).unwrap();
        assert_eq!(response.status, 200);
        assert!(response.is_success());
        assert_eq!(response.retry_after, None);
        assert_eq!(response.body, b"a\nb");
    }

    #[test]
    fn http_date_retry_after_is_ignored() {
        let response = parse_output(b"x\n503 Wed, 21 Oct 2015 07:28:00 GMT".to_vec()).unwrap();
        assert_eq!(response.status, 503);
        assert_eq!(response.retry_after, None);
    }

    #[test]
    fn config_escapes_quotes_and_keeps_credentials_off_argv() {
        let config = String::from_utf8(
            curl_config("https://x.test/v1", "sk-secret", Some(r#"{"a":"b\"c"}"#)).unwrap(),
        )
        .unwrap();
        assert!(config.contains("header = \"Authorization: Bearer sk-secret\"\n"));
        assert!(
            config.contains(r#"data-binary = "{\"a\":\"b\\\"c\"}""#),
            "{config}"
        );
        assert!(config.contains("url = \"https://x.test/v1\"\n"));
        assert!(config.contains("request = \"POST\"\n"));
    }

    #[test]
    fn get_requests_carry_no_body_or_method_override() {
        let config =
            String::from_utf8(curl_config("https://x.test/v1/key", "k", None).unwrap()).unwrap();
        assert!(!config.contains("request ="));
        assert!(!config.contains("data-binary"));
        assert!(config.contains("Authorization: Bearer k"));
    }

    #[test]
    fn config_rejects_raw_control_characters() {
        assert!(curl_config("https://x.test", "k", Some("\u{7}")).is_err());
    }
}
