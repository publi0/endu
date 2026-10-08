//! Local Azure resource coordinates. Credentials stay in the provider Keychain item.
use crate::openrouter::Config;
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MicrosoftConfig {
    pub endpoint: String,
    pub streaming_endpoint: String,
    pub deployment: String,
}

impl MicrosoftConfig {
    /// Speech uses the same resource and key as batch. Retain explicitly
    /// configured Foundry Realtime deployments for existing installations.
    pub fn uses_speech_streaming(&self) -> bool {
        self.deployment.is_empty()
    }

    pub fn validate(&self) -> Result<(), String> {
        for (endpoint, streaming) in [(&self.endpoint, false), (&self.streaming_endpoint, true)] {
            if !endpoint.is_empty() {
                resource_root(endpoint, streaming)?;
            }
        }
        if self.deployment.len() > 128
            || !self
                .deployment
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        {
            return Err(
                "Use the Azure deployment name (letters, numbers, hyphens, underscores or dots)."
                    .into(),
            );
        }
        Ok(())
    }
}

fn resource_root(value: &str, streaming: bool) -> Result<Url, String> {
    let suffix = if streaming {
        ".services.ai.azure.com"
    } else {
        ".cognitiveservices.azure.com"
    };
    let message = format!("Enter an HTTPS Azure resource root ending in {suffix} in Providers.");
    // Reject URL normalization tricks before parsing, and require one Azure resource label.
    if value.len() > 512 || value.contains('\\') || value.chars().any(char::is_whitespace) {
        return Err(message);
    }
    let parsed = Url::parse(value).map_err(|_| message.clone())?;
    let resource = parsed.host_str().and_then(|host| host.strip_suffix(suffix));
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
        || !resource.is_some_and(|r| {
            !r.is_empty()
                && r.len() <= 63
                && !r.starts_with('-')
                && !r.ends_with('-')
                && r.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    {
        return Err(message);
    }
    Ok(parsed)
}

/// Speech custom-domain WebSockets answer a valid key with a redirect to
/// `{region}.stt.speech.microsoft.com`. Hex never forwards a key across a
/// redirect, so it exchanges the key for a short-lived token on the configured
/// resource and connects to the fixed regional host with that token.
#[derive(Clone)]
pub(crate) struct SpeechSession {
    pub url: Url,
    pub token: String,
}

/// Tokens last ten minutes; reuse one for eight so a dictation never starts
/// with a token about to expire.
const SPEECH_TOKEN_REUSE: std::time::Duration = std::time::Duration::from_secs(8 * 60);

struct CachedSpeechToken {
    endpoint: String,
    key_fingerprint: u64,
    issued: std::time::Instant,
    region: String,
    token: String,
}

static SPEECH_TOKEN: std::sync::Mutex<Option<CachedSpeechToken>> = std::sync::Mutex::new(None);

fn key_fingerprint(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

/// The regional Speech WebSocket for `custom_domain_url`, keeping its query
/// (format and language). The host is built from a validated region name only.
pub(crate) fn regional_speech_url(custom_domain_url: &Url, region: &str) -> Option<Url> {
    if !(2..=40).contains(&region.len())
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return None;
    }
    let mut url = Url::parse(&format!(
        "wss://{region}.stt.speech.microsoft.com/speech/universal/v2"
    ))
    .ok()?;
    url.set_query(custom_domain_url.query());
    if let Some(domain) = custom_domain_url.host_str() {
        url.query_pairs_mut()
            .append_pair("Ocp-Apim-Custom-Domain-Name", domain);
    }
    Some(url)
}

/// The `region` claim of a Speech STS token. Only the payload is read; the
/// token itself is opaque to Hex and is never logged.
pub(crate) fn token_region(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("region")?.as_str().map(str::to_ascii_lowercase)
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Some(out)
}

/// Exchanges the key on the configured resource (HTTPS, no redirects) and
/// returns the regional URL plus bearer token. Blocking: call off capture.
pub(crate) fn speech_session(
    config: &Config,
    custom_domain_url: &Url,
    key: &str,
    timeout: std::time::Duration,
) -> Result<SpeechSession, &'static str> {
    let mut sts = microsoft_endpoint(config, false)
        .map_err(|_| "Configure the Microsoft resource endpoint in Providers.")?;
    sts.set_path("/sts/v1.0/issueToken");
    let endpoint = sts.to_string();
    let fingerprint = key_fingerprint(key);
    {
        let cached = SPEECH_TOKEN
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(cached) = cached.as_ref()
            && cached.endpoint == endpoint
            && cached.key_fingerprint == fingerprint
            && cached.issued.elapsed() < SPEECH_TOKEN_REUSE
        {
            return regional_speech_url(custom_domain_url, &cached.region)
                .map(|url| SpeechSession {
                    url,
                    token: cached.token.clone(),
                })
                .ok_or("Microsoft returned an invalid Speech region.");
        }
    }
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .https_only(true)
            .timeout_global(Some(timeout))
            .build(),
    );
    let mut response = agent
        .post(&endpoint)
        .header("Ocp-Apim-Subscription-Key", key)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send_empty()
        .map_err(|_| "Could not reach the Microsoft resource for a Speech token.")?;
    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Err("Microsoft rejected the key for Speech."),
        _ => return Err("Microsoft could not issue a Speech token."),
    }
    let token = response
        .body_mut()
        .with_config()
        .limit(16 * 1024)
        .read_to_string()
        .map_err(|_| "Microsoft returned an invalid Speech token.")?
        .trim()
        .to_owned();
    let region = token_region(&token).ok_or("Microsoft returned an invalid Speech token.")?;
    let url = regional_speech_url(custom_domain_url, &region)
        .ok_or("Microsoft returned an invalid Speech region.")?;
    *SPEECH_TOKEN
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(CachedSpeechToken {
        endpoint,
        key_fingerprint: fingerprint,
        issued: std::time::Instant::now(),
        region,
        token: token.clone(),
    });
    Ok(SpeechSession { url, token })
}

/// Return only a validated root; adapters append their documented protocol paths.
pub fn microsoft_endpoint(config: &Config, streaming: bool) -> Result<Url, String> {
    config.microsoft.validate()?;
    let realtime = streaming && !config.microsoft.uses_speech_streaming();
    resource_root(
        if realtime {
            &config.microsoft.streaming_endpoint
        } else {
            &config.microsoft.endpoint
        },
        realtime,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_sessions_use_the_token_region_on_the_fixed_microsoft_host() {
        // {"region":"southeastasia"} as a synthetic, unsigned JWT payload.
        let token = "e30.eyJyZWdpb24iOiJzb3V0aGVhc3Rhc2lhIn0.sig";
        assert_eq!(token_region(token).as_deref(), Some("southeastasia"));
        assert_eq!(token_region("not-a-token"), None);
        let custom = Url::parse(
            "wss://fixture.cognitiveservices.azure.com/stt/speech/universal/v2?format=simple&language=pt-BR",
        )
        .unwrap();
        let regional = regional_speech_url(&custom, "southeastasia").unwrap();
        assert_eq!(
            regional.as_str(),
            "wss://southeastasia.stt.speech.microsoft.com/speech/universal/v2?format=simple&language=pt-BR&Ocp-Apim-Custom-Domain-Name=fixture.cognitiveservices.azure.com"
        );
        // A region can never steer the connection to another host.
        for hostile in [
            "evil.com/",
            "a.b",
            "x@y",
            "",
            "UPPER",
            "east us",
            &"a".repeat(41),
        ] {
            assert!(regional_speech_url(&custom, hostile).is_none(), "{hostile}");
        }
    }

    #[test]
    fn azure_roots_reject_credentials_and_lookalike_hosts() {
        for bad in [
            "https://resource.cognitiveservices.azure.com.evil.test",
            "https://cognitiveservices.azure.com",
            "https://a.b.cognitiveservices.azure.com",
            "https://user:secret@resource.cognitiveservices.azure.com",
            "http://resource.cognitiveservices.azure.com",
            "https://resource.cognitiveservices.azure.com:444",
            "https://resource.cognitiveservices.azure.com/path",
            "https://resource.cognitiveservices.azure.com?key=secret",
            "https://resource.cognitiveservices.azure.com#fragment",
            "https://resource.cognitiveservices.azure.com\\@evil.test",
            "https:// resource.cognitiveservices.azure.com",
            "https://-resource.cognitiveservices.azure.com",
        ] {
            let error = resource_root(bad, false).unwrap_err();
            assert!(!error.contains("secret"));
        }
        assert!(resource_root("https://fixture.cognitiveservices.azure.com/", false).is_ok());
        assert!(resource_root("https://fixture.services.ai.azure.com", true).is_ok());
        assert!(resource_root("https://fixture.services.ai.azure.com", false).is_err());
    }

    #[test]
    fn empty_connections_migrate_but_cannot_transcribe() {
        let old: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(old.microsoft, MicrosoftConfig::default());
        assert!(old.microsoft.validate().is_ok());
        assert!(microsoft_endpoint(&old, false).is_err());
        assert!(microsoft_endpoint(&old, true).is_err());
        let mut config = old;
        config.microsoft.endpoint = "https://fixture.cognitiveservices.azure.com".into();
        assert!(microsoft_endpoint(&config, false).is_ok());
        config.microsoft.streaming_endpoint = "https://fixture.services.ai.azure.com".into();
        assert_eq!(
            microsoft_endpoint(&config, true).unwrap(),
            microsoft_endpoint(&config, false).unwrap()
        );
        config.microsoft.deployment = "MAI-Transcribe-2-Streaming".into();
        assert!(microsoft_endpoint(&config, true).is_ok());
        config.microsoft.deployment = "injected\nname".into();
        assert!(config.microsoft.validate().is_err());
    }
}
