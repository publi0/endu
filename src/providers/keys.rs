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
    /// The item exists but macOS refused it (Deny, Cancel, or no reply). Asking
    /// again on a timer would stack Keychain prompts, so only a user action
    /// (Test, save, remove) clears this.
    Unavailable,
}

/// Outcome of reading one secret. Only `Found` ever involved decrypting it.
pub(crate) enum KeychainRead {
    Found(String),
    Missing,
    Unavailable,
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
        || !key.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.')
                || (provider == Provider::Microsoft && matches!(byte, b'+' | b'/' | b'='))
                // Meta Model API keys use LLM|account|secret; these pipes are
                // data in HTTPS headers/JSON, never shell arguments.
                || (provider == Provider::Meta && byte == b'|')
        })
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
        Some(CachedKey::Unavailable) => bail!(unavailable_message(provider.label())),
        _ => {}
    }
    match read_keychain(provider) {
        KeychainRead::Found(key) => {
            cache.insert(provider, CachedKey::Found(key.clone()));
            Ok(key)
        }
        KeychainRead::Missing => {
            cache.insert(provider, CachedKey::Missing(Instant::now()));
            bail!(
                "{} API key not found. Add it in Providers.",
                provider.label()
            )
        }
        KeychainRead::Unavailable => {
            cache.insert(provider, CachedKey::Unavailable);
            bail!(unavailable_message(provider.label()))
        }
    }
}

pub(crate) fn unavailable_message(provider: &str) -> String {
    format!(
        "macOS did not allow Hex to read the {provider} key. Open Providers and press Test to allow access."
    )
}

/// Last four characters, for recognising a key without showing it.
pub(crate) fn key_suffix(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
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
    // Status is shown on every launch and refresh, so it must never decrypt the
    // secret: after an update macOS would ask for the login password per item.
    if let Some(CachedKey::Found(key)) = CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&provider)
    {
        return KeyStatus::Keychain(key_suffix(key));
    }
    if keychain_item_exists(provider.id()) {
        KeyStatus::Keychain(String::new())
    } else {
        KeyStatus::Missing
    }
}

/// Forget a refused read so the next explicit user action may ask macOS again.
fn retry_unavailable(provider: Provider) {
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if matches!(cache.get(&provider), Some(CachedKey::Unavailable)) {
        cache.remove(&provider);
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
    if !matches!(read_keychain(provider), KeychainRead::Found(saved) if saved == key) {
        bail!(
            "The Keychain did not confirm the saved {} key.",
            provider.label()
        );
    }
    CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(provider, CachedKey::Found(key.to_owned()));
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
    if keychain_item_exists(provider.id()) {
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

fn read_keychain(provider: Provider) -> KeychainRead {
    match read_secret(provider.id()) {
        KeychainRead::Found(key) => validate_key(provider, &key)
            .map_or(KeychainRead::Missing, |key| {
                KeychainRead::Found(key.to_owned())
            }),
        other => other,
    }
}

/// Decrypts one secret, which may show a Keychain prompt. Call only when the
/// key is needed for a request or an explicit user action.
#[cfg(target_os = "macos")]
pub(crate) fn read_secret(account: &str) -> KeychainRead {
    match security_framework::passwords::get_generic_password(openrouter::KEYCHAIN_SERVICE, account)
    {
        Ok(bytes) => String::from_utf8(bytes)
            .ok()
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty())
            .map_or(KeychainRead::Missing, KeychainRead::Found),
        Err(error) if error.code() == security_framework_sys::base::errSecItemNotFound => {
            KeychainRead::Missing
        }
        Err(_) => KeychainRead::Unavailable,
    }
}

/// Whether an item exists, from its attributes alone. Attributes are not
/// protected by the item's access list, so this never shows a prompt.
#[cfg(target_os = "macos")]
pub(crate) fn keychain_item_exists(account: &str) -> bool {
    use security_framework::item::{ItemClass, ItemSearchOptions};
    ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(openrouter::KEYCHAIN_SERVICE)
        .account(account)
        .load_attributes(true)
        .limit(1)
        .search()
        .is_ok_and(|results| !results.is_empty())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn read_secret(_account: &str) -> KeychainRead {
    KeychainRead::Missing
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn keychain_item_exists(_account: &str) -> bool {
    false
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
        Provider::Microsoft => ("Ocp-Apim-Subscription-Key", key.into()),
        Provider::Google => ("x-goog-api-key", key.into()),
        Provider::OpenRouter | Provider::OpenAi | Provider::Grok | Provider::Meta => {
            ("Authorization", format!("Bearer {key}"))
        }
    }
}

fn check_endpoint(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAi => "https://api.openai.com/v1/models",
        Provider::Grok => "https://api.x.ai/v1/models",
        Provider::Meta => "https://api.meta.ai/v1/models",
        Provider::Google => "https://generativelanguage.googleapis.com/v1beta/models",
        Provider::Microsoft => {
            unreachable!("Azure token checks require a validated resource endpoint")
        }
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
    retry_unavailable(provider);
    // Resolve and validate destination before looking up any credentials.
    let endpoint = if provider == Provider::Microsoft {
        if config.microsoft.endpoint.is_empty() {
            bail!("Enter the Microsoft resource endpoint in Providers to test this key.");
        }
        let mut root = super::microsoft_endpoint(config, false).map_err(|e| eyre!("{e}"))?;
        root.set_path("/sts/v1.0/issueToken");
        root.to_string()
    } else {
        check_endpoint(provider).into()
    };
    openrouter::http::validate_url(&endpoint)?;
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
    let response = if provider == Provider::Microsoft {
        // The documented Speech STS exchange checks the key without sending audio.
        agent
            .post(&endpoint)
            .header(name, value)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send_empty()
    } else {
        agent.get(&endpoint).header(name, value).call()
    };
    let mut response =
        response.map_err(|_| eyre!("Could not reach {} to check the key.", provider.label()))?;
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
    if provider == Provider::Microsoft {
        // Never return or cache the short-lived token from the check.
        if body.is_empty() {
            bail!("The provider returned an empty key-check response.");
        }
        return Ok(message);
    }
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
    fn a_refused_keychain_read_waits_for_an_explicit_retry() {
        let provider = Provider::ElevenLabs;
        CACHE
            .lock()
            .unwrap()
            .insert(provider, CachedKey::Unavailable);
        // Status refreshes never clear a refusal; only an explicit retry does.
        assert!(matches!(
            CACHE.lock().unwrap().get(&provider),
            Some(CachedKey::Unavailable)
        ));
        retry_unavailable(provider);
        assert!(CACHE.lock().unwrap().get(&provider).is_none());
        // A found key is never discarded by a retry.
        CACHE
            .lock()
            .unwrap()
            .insert(provider, CachedKey::Found("fixture-key-0000".into()));
        retry_unavailable(provider);
        assert!(matches!(
            CACHE.lock().unwrap().get(&provider),
            Some(CachedKey::Found(_))
        ));
        CACHE.lock().unwrap().remove(&provider);
        assert_eq!(key_suffix("fixture-key-abcd"), "abcd");
        assert!(unavailable_message("Deepgram").contains("press Test"));
    }

    #[test]
    fn meta_accepts_its_documented_pipe_separated_key_without_relaxing_other_providers() {
        // https://dev.meta.ai/docs/authentication
        let key = "LLM|123456789012345|fixture-key-0123456789";
        assert_eq!(validate_key(Provider::Meta, key).unwrap(), key);
        for provider in Provider::ALL {
            if provider != Provider::Meta {
                assert!(validate_key(provider, key).is_err());
            }
        }
        for invalid in [
            "LLM|123456789012345|PRIVATE_MARKER\nheader",
            "LLM|123456789012345|PRIVATE_MARKER`id`",
            "LLM|123456789012345|PRIVATE_MARKER;id",
        ] {
            let error = validate_key(Provider::Meta, invalid).unwrap_err();
            assert!(!error.to_string().contains("PRIVATE_MARKER"));
        }
    }
    #[test]
    fn new_credentials_use_only_their_provider_header_and_destination() {
        assert_eq!(
            authorization(Provider::Meta, "fixture"),
            ("Authorization", "Bearer fixture".into())
        );
        assert_eq!(
            check_endpoint(Provider::Meta),
            "https://api.meta.ai/v1/models"
        );
        assert_eq!(Provider::Meta.env(), "MODEL_API_KEY");
        assert_eq!(
            authorization(Provider::Microsoft, "fixture"),
            ("Ocp-Apim-Subscription-Key", "fixture".into())
        );
        assert_eq!(
            authorization(Provider::Google, "fixture"),
            ("x-goog-api-key", "fixture".into())
        );
        assert_eq!(
            authorization(Provider::Grok, "fixture"),
            ("Authorization", "Bearer fixture".into())
        );
        assert_eq!(
            check_endpoint(Provider::Google),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
        assert_eq!(check_endpoint(Provider::Grok), "https://api.x.ai/v1/models");
        assert_eq!(Provider::Microsoft.env(), "AZURE_MAI_API_KEY");
        assert_eq!(Provider::Google.env(), "GEMINI_API_KEY");
        assert_eq!(Provider::Grok.env(), "XAI_API_KEY");
    }

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
        for provider in Provider::ALL
            .into_iter()
            .filter(|p| *p != Provider::OpenRouter)
        {
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
