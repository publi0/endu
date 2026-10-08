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
