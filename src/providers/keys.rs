//! Separate credentials for direct providers. No key enters argv or diagnostics.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::{bail, eyre};

use super::Provider;
use crate::openrouter::{self, Config, KeyStatus};

enum CachedKey {
    Found(String),
    Missing(Instant),
}

static CACHE: LazyLock<Mutex<BTreeMap<Provider, CachedKey>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
const RECHECK_MISSING: Duration = Duration::from_secs(5);

pub fn validate_key(provider: Provider, key: &str) -> Result<&str> {
    if provider == Provider::OpenRouter {
        return openrouter::validate_key(key);
    }
    let key = key.trim();
    if !(16..=4096).contains(&key.len())
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("The {} key has an invalid format.", provider.label());
    }
    Ok(key)
}

pub fn api_key(provider: Provider, config: &Config) -> Result<String> {
    if cfg!(test) {
        bail!("Provider credentials are disabled in unit tests.");
    }
    if provider == Provider::OpenRouter {
        return openrouter::api_key(config);
    }
    if let Ok(key) = std::env::var(provider.env())
        && !key.trim().is_empty()
    {
        return validate_key(provider, &key).map(str::to_owned);
    }
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    match cache.get(&provider) {
        Some(CachedKey::Found(key)) => return Ok(key.clone()),
        Some(CachedKey::Missing(at)) if at.elapsed() < RECHECK_MISSING => {
            bail!(
                "{} API key not found. Add it in Providers.",
                provider.label()
            );
        }
        _ => {}
    }
    match read_keychain(provider) {
        Some(key) => {
            cache.insert(provider, CachedKey::Found(key.clone()));
            Ok(key)
        }
        None => {
            cache.insert(provider, CachedKey::Missing(Instant::now()));
            bail!(
                "{} API key not found. Add it in Providers.",
                provider.label()
            )
        }
    }
}

pub fn key_status(provider: Provider, config: &Config) -> KeyStatus {
    if cfg!(test) {
        return KeyStatus::Missing;
    }
    if provider == Provider::OpenRouter {
        return openrouter::key_status(config);
    }
    if std::env::var(provider.env()).is_ok_and(|key| !key.trim().is_empty()) {
        return KeyStatus::Environment;
    }
    invalidate(provider);
    match api_key(provider, config) {
        Ok(key) => KeyStatus::Keychain(
            key.chars()
                .rev()
                .take(4)
                .collect::<String>()
                .chars()
                .rev()
                .collect(),
        ),
        Err(_) => KeyStatus::Missing,
    }
}

pub(crate) fn invalidate(provider: Provider) {
    if provider == Provider::OpenRouter {
        openrouter::forget_cached_key();
    } else {
        CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&provider);
    }
}

pub fn store_keychain_key(provider: Provider, key: &str) -> Result<()> {
    if cfg!(test) {
        bail!("Provider credentials are disabled in unit tests.");
    }
    if provider == Provider::OpenRouter {
        return openrouter::store_keychain_key(key);
    }
    let key = validate_key(provider, key)?;
    store_native(provider, key)?;
    invalidate(provider);
    if read_keychain(provider).as_deref() != Some(key) {
        bail!(
            "The Keychain did not confirm the saved {} key.",
            provider.label()
        );
    }
    Ok(())
}

pub fn delete_keychain_key(provider: Provider) -> Result<()> {
    if cfg!(test) {
        bail!("Provider credentials are disabled in unit tests.");
    }
    if provider == Provider::OpenRouter {
        return openrouter::delete_keychain_key();
    }
    delete_native(provider)?;
    invalidate(provider);
    if read_keychain(provider).is_some() {
        bail!("The {} key is still in the Keychain.", provider.label());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn store_native(provider: Provider, key: &str) -> Result<()> {
    security_framework::passwords::set_generic_password(
        openrouter::KEYCHAIN_SERVICE,
        provider.id(),
        key.as_bytes(),
    )
    .map_err(|_| eyre!("The Keychain did not accept the key."))
}

#[cfg(target_os = "macos")]
fn delete_native(provider: Provider) -> Result<()> {
    let result = security_framework::passwords::delete_generic_password(
        openrouter::KEYCHAIN_SERVICE,
        provider.id(),
    );
    if let Err(error) = result
        && error.code() != security_framework_sys::base::errSecItemNotFound
    {
        bail!("Could not remove the key from the Keychain.");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn read_keychain(provider: Provider) -> Option<String> {
    let bytes = security_framework::passwords::get_generic_password(
        openrouter::KEYCHAIN_SERVICE,
        provider.id(),
    )
    .ok()?;
    let key = String::from_utf8(bytes).ok()?;
    validate_key(provider, &key).ok().map(str::to_owned)
}

#[cfg(not(target_os = "macos"))]
fn read_keychain(_provider: Provider) -> Option<String> {
    None
}
#[cfg(not(target_os = "macos"))]
fn store_native(_provider: Provider, _key: &str) -> Result<()> {
    bail!(
        "The Keychain is only available on macOS; use the provider's API key environment variable."
    )
}
#[cfg(not(target_os = "macos"))]
fn delete_native(_provider: Provider) -> Result<()> {
    bail!("The Keychain is only available on macOS.")
}

pub(crate) fn authorization(provider: Provider, key: &str) -> (&'static str, String) {
    match provider {
        Provider::Deepgram => ("Authorization", format!("Token {key}")),
        Provider::ElevenLabs => ("xi-api-key", key.into()),
        Provider::OpenRouter | Provider::OpenAi => ("Authorization", format!("Bearer {key}")),
    }
}

fn check_endpoint(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAi => "https://api.openai.com/v1/models",
        Provider::Deepgram => "https://api.deepgram.com/v1/auth/token",
        Provider::ElevenLabs => "https://api.elevenlabs.io/v1/user",
        Provider::OpenRouter => unreachable!("OpenRouter key checks retain their existing adapter"),
    }
}

pub fn check_key(provider: Provider, config: &Config) -> Result<String> {
    if cfg!(test) {
        bail!("Provider credentials are disabled in unit tests.");
    }
    if provider == Provider::OpenRouter {
        return openrouter::check_key(config);
    }
    let key = api_key(provider, config)?;
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .https_only(true)
            .timeout_global(Some(Duration::from_secs(15)))
            .build(),
    );
    let (name, value) = authorization(provider, &key);
    let mut response = agent
        .get(check_endpoint(provider))
        .header(name, value)
        .call()
        .map_err(|_| eyre!("Could not reach {} to check the key.", provider.label()))?;
    let status = response.status().as_u16();
    if status == 401 {
        invalidate(provider);
    }
    let message = check_status(provider, status)?;
    let body = response
        .body_mut()
        .with_config()
        .limit(2 * 1024 * 1024)
        .read_to_vec()
        .map_err(|_| eyre!("The key-check response could not be read safely."))?;
    if !serde_json::from_slice::<serde_json::Value>(&body).is_ok_and(|body| body.is_object()) {
        bail!("The provider returned an invalid key-check response.");
    }
    Ok(message)
}

fn check_status(provider: Provider, status: u16) -> Result<String> {
    match status {
        200..=299 => Ok("Key accepted. Speech-to-text access is checked when transcribing.".into()),
        401 => Err(eyre!("{} rejected the key (HTTP 401).", provider.label())),
        403 => Err(eyre!(
            "{} denied this key check (HTTP 403). The key may lack read permission; transcription access has not been tested.",
            provider.label()
        )),
        429 => Err(eyre!(
            "The key check was rate limited (HTTP 429). Try again later."
        )),
        _ => Err(eyre!(
            "{} key check failed (HTTP {status}).",
            provider.label()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_provider_entrypoints_stop_before_credentials_or_network_in_tests() {
        let config = Config::default();
        for provider in Provider::ALL {
            assert!(matches!(key_status(provider, &config), KeyStatus::Missing));
            for error in [
                api_key(provider, &config).unwrap_err(),
                check_key(provider, &config).unwrap_err(),
                store_keychain_key(provider, "synthetic_fixture_only").unwrap_err(),
                delete_keychain_key(provider).unwrap_err(),
            ] {
                assert_eq!(
                    error.to_string(),
                    "Provider credentials are disabled in unit tests."
                );
            }
        }
    }
    #[test]
    fn providers_have_distinct_accounts_and_authentication_contracts() {
        assert_ne!(Provider::OpenAi.id(), Provider::OpenRouter.id());
        assert_eq!(
            authorization(Provider::Deepgram, "fixture"),
            ("Authorization", "Token fixture".into())
        );
        assert_eq!(
            authorization(Provider::ElevenLabs, "fixture"),
            ("xi-api-key", "fixture".into())
        );
        assert_eq!(
            check_endpoint(Provider::Deepgram),
            "https://api.deepgram.com/v1/auth/token"
        );
    }
    #[test]
    fn invalid_keys_cannot_inject_commands_and_errors_do_not_echo_them() {
        for provider in [Provider::OpenAi, Provider::Deepgram, Provider::ElevenLabs] {
            let error = validate_key(provider, "PRIVATE_MARKER\nquit").unwrap_err();
            assert!(!error.to_string().contains("PRIVATE_MARKER"));
            assert!(validate_key(provider, "sk-fixture_only_0123456789").is_ok());
            assert!(
                check_status(provider, 401)
                    .unwrap_err()
                    .to_string()
                    .contains("rejected")
            );
            assert!(
                check_status(provider, 403)
                    .unwrap_err()
                    .to_string()
                    .contains("read permission")
            );
        }
    }
}
