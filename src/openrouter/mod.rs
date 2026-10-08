//! Cloud transcription through OpenRouter with an ordered model fallback
//! chain, silence trimming, and per-dictation reports for History.
//!
//! Configuration is a JSON file in Application Support (`openrouter.json`),
//! re-read on every request so edits apply without restarting. The API key
//! comes from `OPENROUTER_API_KEY`, the config file, or the macOS Keychain
//! (`security add-generic-password -s hex-openrouter -a openrouter -w`).

pub mod catalog;
pub mod form;
pub(crate) mod http;
pub mod report;
#[cfg(target_os = "macos")]
pub mod settings_view;
pub mod stats;
pub mod stats_dashboard;
#[cfg(target_os = "macos")]
pub mod stats_view;
pub mod transcribe;
pub(crate) mod vad;
pub mod vocabulary_support;

pub use report::{AudioTrim, StepReport};

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail};
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "openrouter.json";
pub const KEYCHAIN_SERVICE: &str = "hex-openrouter";
pub const KEYCHAIN_ACCOUNT: &str = "openrouter";
pub const API_KEY_ENV: &str = "OPENROUTER_API_KEY";
pub const AUTO_LANGUAGE: &str = "auto";

/// Languages offered in Models, as ISO-639-1 codes for the API.
pub const LANGUAGES: &[(&str, &str)] = &[
    (AUTO_LANGUAGE, "Auto-detect"),
    ("pt", "Portuguese"),
    ("en", "English"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("de", "German"),
    ("it", "Italian"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
    ("ru", "Russian"),
    ("uk", "Ukrainian"),
    ("tr", "Turkish"),
    ("ar", "Arabic"),
    ("hi", "Hindi"),
    ("zh", "Chinese"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("vi", "Vietnamese"),
    ("id", "Indonesian"),
    ("sv", "Swedish"),
    ("da", "Danish"),
    ("fi", "Finnish"),
    ("cs", "Czech"),
    ("el", "Greek"),
    ("ro", "Romanian"),
    ("hu", "Hungarian"),
];

pub fn language_name(code: &str) -> &str {
    LANGUAGES
        .iter()
        .find_map(|(candidate, name)| (*candidate == code).then_some(*name))
        .unwrap_or(code)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Config {
    /// Optional plaintext key. Prefer the Keychain; see the module docs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// OpenRouter-compatible API root.
    pub base_url: String,
    pub transcription: TranscriptionConfig,
    pub microsoft: crate::providers::MicrosoftConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct TranscriptionConfig {
    pub model_options: std::collections::BTreeMap<String, crate::providers::ModelOptions>,
    /// Tried in order. Any failure (transport, timeout, HTTP error, invalid
    /// response) moves on to the next model.
    pub models: Vec<String>,
    /// ISO-639-1 code sent to the provider, or `auto` to let it detect.
    pub language: String,
    /// Deadline for one request to one model.
    pub attempt_timeout_seconds: u64,
    /// Deadline for the whole fallback chain of one audio chunk.
    pub total_timeout_seconds: u64,
    /// Long recordings are split into chunks of at most this length, cut at
    /// the quietest point near the boundary.
    pub chunk_seconds: u64,
    /// A 429 whose Retry-After is at most this long is retried once on the
    /// same model before falling back. Zero disables the retry.
    pub rate_limit_retry_max_wait_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Cut silence before sending: leading and trailing silence is removed,
    /// long pauses are shortened, and a clip with no speech is not sent.
    pub trim_silence: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: "https://openrouter.ai/api/v1".into(),
            transcription: TranscriptionConfig::default(),
            microsoft: Default::default(),
        }
    }
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            model_options: Default::default(),
            models: vec![
                "openai/whisper-large-v3-turbo".into(),
                "openai/gpt-4o-mini-transcribe".into(),
                "mistralai/voxtral-mini-transcribe".into(),
            ],
            language: AUTO_LANGUAGE.into(),
            attempt_timeout_seconds: 30,
            total_timeout_seconds: 90,
            chunk_seconds: 120,
            rate_limit_retry_max_wait_ms: 2_000,
            temperature: Some(0.0),
            trim_silence: true,
        }
    }
}

impl Config {
    pub fn attempt_timeout(&self) -> Duration {
        Duration::from_secs(self.transcription.attempt_timeout_seconds.max(1))
    }

    pub fn total_timeout(&self) -> Duration {
        Duration::from_secs(self.transcription.total_timeout_seconds.max(1))
    }

    pub(crate) fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), path)
    }
}

pub fn config_path() -> Result<PathBuf> {
    Ok(crate::app_paths::support_dir()?.join(CONFIG_FILE))
}

/// Read the configuration, writing a commented-by-example template on first
/// use so there is always a file to edit.
pub fn load_config() -> Result<Config> {
    let path = config_path()?;
    let config = load_config_at(&path)?;
    config
        .microsoft
        .validate()
        .map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
    crate::providers::validate_profiles(&config.transcription.model_options)
        .map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
    crate::providers::apply_runtime(&config);
    Ok(config)
}

pub(crate) fn load_config_at(path: &Path) -> Result<Config> {
    with_config_edits(|| load_config_at_unlocked(path))
}

// Call only while CONFIG_EDITS is held. Public reads must not observe the
// Models half of a preferences import that may still roll back.
fn load_config_at_unlocked(path: &Path) -> Result<Config> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .wrap_err_with(|| format!("invalid OpenRouter config at {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let config = Config::default();
            if let Err(error) = write_template(path, &config) {
                tracing::warn!(%error, path = %path.display(), "could not write OpenRouter config template");
            }
            Ok(config)
        }
        Err(error) => Err(error)
            .wrap_err_with(|| format!("could not read OpenRouter config at {}", path.display())),
    }
}

fn write_template(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(serde_json::to_string_pretty(config)?.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

static CONFIG_EDITS: Mutex<()> = Mutex::new(());
static CONFIG_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Apply one edit to the latest readable configuration. Serialize application
/// writers so a Keychain migration cannot overwrite a preferences edit.
pub fn update_config(edit: impl FnOnce(&Config) -> Result<Config>) -> Result<Config> {
    update_config_at(&config_path()?, edit)
}

fn update_config_at(path: &Path, edit: impl FnOnce(&Config) -> Result<Config>) -> Result<Config> {
    with_config_edits(|| {
        let config = edit(&load_config_at_unlocked(path)?)?;
        save_config_at(path, &config)?;
        crate::providers::apply_runtime(&config);
        Ok(config)
    })
}

/// Hold the same writer lock across a multi-file preferences import and rollback.
pub(crate) fn with_config_edits<T>(edit: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = CONFIG_EDITS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    edit()
}

fn config_temporary_path(path: &Path) -> PathBuf {
    let sequence = CONFIG_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("json.{}.{}.tmp", std::process::id(), sequence))
}

pub(crate) fn save_config_at(path: &Path, config: &Config) -> Result<()> {
    config
        .microsoft
        .validate()
        .map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
    crate::providers::validate_profiles(&config.transcription.model_options)
        .map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
    let parent = path
        .parent()
        .ok_or_else(|| color_eyre::eyre::eyre!("config path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = config_temporary_path(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(serde_json::to_string_pretty(config)?.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
            .wrap_err_with(|| format!("could not save {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Where the API key in use comes from, for display. Never holds the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyStatus {
    Environment,
    ConfigFile,
    /// Stored in the Keychain; the last four characters, for recognition.
    Keychain(String),
    Missing,
}

/// Blocking: may run `security`. Call off the UI thread.
pub fn key_status(config: &Config) -> KeyStatus {
    if std::env::var(API_KEY_ENV).is_ok_and(|key| !key.trim().is_empty()) {
        return KeyStatus::Environment;
    }
    if config
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
    {
        return KeyStatus::ConfigFile;
    }
    forget_cached_key();
    match api_key(config) {
        Ok(key) => KeyStatus::Keychain(key_suffix(&key)),
        Err(_) => KeyStatus::Missing,
    }
}

fn key_suffix(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

/// OpenRouter keys are URL-safe tokens; anything else is almost certainly a
/// paste mistake (and would need quoting for `security`).
pub fn validate_key(key: &str) -> Result<&str> {
    let key = key.trim();
    if key.len() < 16 {
        bail!("That does not look like an OpenRouter key.");
    }
    if !key
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("The key contains unexpected characters.");
    }
    Ok(key)
}

/// Store the key in the login Keychain. Blocking.
///
/// Uses Security.framework directly so the item's ACL binds to Hex's signed
/// identity instead of the globally invokable `/usr/bin/security` helper.
#[cfg(target_os = "macos")]
pub fn store_keychain_key(key: &str) -> Result<()> {
    let key = validate_key(key)?;
    security_framework::passwords::set_generic_password(
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        key.as_bytes(),
    )
    .map_err(|error| color_eyre::eyre::eyre!("The Keychain did not accept the key: {error}"))?;
    forget_cached_key();
    if keychain_key().as_deref() != Some(key) {
        bail!("The Keychain did not retain the key.");
    }
    Ok(())
}

/// Remove the key from the login Keychain. Blocking.
#[cfg(target_os = "macos")]
pub fn delete_keychain_key() -> Result<()> {
    let result =
        security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
    forget_cached_key();
    if let Err(error) = &result
        && error.code() != security_framework_sys::base::errSecItemNotFound
    {
        bail!("The Keychain refused the removal: {error}");
    }
    if keychain_key().is_some() {
        bail!("The key is still in the Keychain.");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn store_keychain_key(_key: &str) -> Result<()> {
    bail!("The Keychain is only available on macOS; set OPENROUTER_API_KEY instead.")
}

#[cfg(not(target_os = "macos"))]
pub fn delete_keychain_key() -> Result<()> {
    bail!("The Keychain is only available on macOS.")
}

/// Ask OpenRouter about the key in use. Blocking; makes one GET request.
pub fn check_key(config: &Config) -> Result<String> {
    let key = api_key(config)?;
    let response = http::get(&config.endpoint("key"), &key, Duration::from_secs(15))?;
    if response.status == 401 {
        forget_cached_key();
        bail!("OpenRouter rejected the key (HTTP 401).");
    }
    if !response.is_success() {
        bail!("HTTP {}: {}", response.status, excerpt(&response.body));
    }
    describe_key(&response.body)
}

fn describe_key(body: &[u8]) -> Result<String> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .wrap_err_with(|| format!("invalid JSON response: {}", excerpt(body)))?;
    let data = value.get("data").unwrap_or(&value);
    let mut parts = vec!["Key works".to_owned()];
    if let Some(label) = data.get("label").and_then(serde_json::Value::as_str) {
        parts.push(format!("label {label}"));
    }
    if let Some(usage) = data.get("usage").and_then(serde_json::Value::as_f64) {
        parts.push(format!("used ${usage:.2}"));
    }
    match data
        .get("limit_remaining")
        .and_then(serde_json::Value::as_f64)
    {
        Some(remaining) => parts.push(format!("${remaining:.2} left")),
        None if data.get("limit").is_some_and(serde_json::Value::is_null) => {
            parts.push("no limit".into())
        }
        None => {}
    }
    Ok(parts.join(" · "))
}

enum CachedKey {
    Found(String),
    /// A recent lookup found nothing; avoid spawning `security` on every UI
    /// readiness check while the user has not stored a key yet.
    Missing(Instant),
}

static KEYCHAIN_KEY: Mutex<Option<CachedKey>> = Mutex::new(None);
const MISSING_KEY_RECHECK: Duration = Duration::from_secs(5);

/// Resolve the API key: environment, then config file, then Keychain.
pub fn api_key(config: &Config) -> Result<String> {
    if let Some(key) = std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.trim().is_empty())
    {
        return Ok(key.trim().to_owned());
    }
    if let Some(key) = config
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        return Ok(key.to_owned());
    }
    let mut cached = KEYCHAIN_KEY
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let recheck = match cached.as_ref() {
        Some(CachedKey::Found(key)) => return Ok(key.clone()),
        Some(CachedKey::Missing(at)) => at.elapsed() >= MISSING_KEY_RECHECK,
        None => true,
    };
    if recheck {
        *cached = Some(match keychain_key() {
            Some(key) => CachedKey::Found(key),
            None => CachedKey::Missing(Instant::now()),
        });
        if let Some(CachedKey::Found(key)) = cached.as_ref() {
            return Ok(key.clone());
        }
    }
    bail!(
        "OpenRouter API key not found. Run: security add-generic-password -s {KEYCHAIN_SERVICE} -a {KEYCHAIN_ACCOUNT} -w"
    )
}

/// Drop the cached Keychain key so a rotated key is picked up after a 401.
pub(crate) fn forget_cached_key() {
    *KEYCHAIN_KEY
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = None;
}

#[cfg(target_os = "macos")]
fn keychain_key() -> Option<String> {
    // Reading through Security.framework keeps the item's ACL bound to Hex's
    // signed identity; no external helper is involved.
    let key =
        security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .ok()?;
    let key = String::from_utf8(key).ok()?;
    let key = key.trim();
    (!key.is_empty()).then(|| key.to_owned())
}

#[cfg(not(target_os = "macos"))]
fn keychain_key() -> Option<String> {
    None
}

/// Short, single-line excerpt of a response body for error messages.
pub(crate) fn excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut excerpt: String = compact.chars().take(200).collect();
    if compact.chars().count() > 200 {
        excerpt.push('…');
    }
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hex-openrouter-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_config_writes_a_template_with_defaults() {
        let dir = temp_dir("template");
        let path = dir.join(CONFIG_FILE);
        let config = load_config_at(&path).unwrap();
        assert_eq!(config, Config::default());
        let written: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(written, Config::default());
        assert!(!fs::read_to_string(&path).unwrap().contains("api_key"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_config_keeps_defaults_for_missing_fields() {
        let config: Config = serde_json::from_str(
            r#"{"transcription":{"models":["a/b"]},"cleanup":{"enabled":true}}"#,
        )
        .unwrap();
        assert_eq!(config.transcription.models, ["a/b"]);
        assert_eq!(config.transcription.attempt_timeout_seconds, 30);
        assert_eq!(config.transcription.language, AUTO_LANGUAGE);
        assert!(config.transcription.trim_silence);
        assert_eq!(config.base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn languages_are_iso_639_1_and_named() {
        assert!(
            LANGUAGES
                .iter()
                .all(|(code, _)| *code == AUTO_LANGUAGE || code.len() == 2)
        );
        assert_eq!(language_name("pt"), "Portuguese");
        assert_eq!(language_name("xx"), "xx");
    }

    #[test]
    fn invalid_config_is_an_error_naming_the_file() {
        let dir = temp_dir("invalid");
        let path = dir.join(CONFIG_FILE);
        fs::write(&path, "{ not json").unwrap();
        let error = load_config_at(&path).unwrap_err().to_string();
        assert!(error.contains(CONFIG_FILE), "{error}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saved_config_round_trips_owner_only() {
        let dir = temp_dir("save");
        let path = dir.join(CONFIG_FILE);
        let mut config = Config::default();
        config.transcription.language = "pt".into();
        config.transcription.models = vec!["x/y".into()];
        save_config_at(&path, &config).unwrap();
        assert_eq!(load_config_at(&path).unwrap(), config);
        assert!(!path.with_extension("json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn editing_latest_config_preserves_other_changes_and_invalid_files() {
        let dir = temp_dir("edits");
        let path = dir.join(CONFIG_FILE);
        let mut external = Config {
            base_url: "https://example.test/v1".into(),
            ..Config::default()
        };
        external.transcription.models = vec!["custom/new".into()];
        save_config_at(&path, &external).unwrap();
        let saved = update_config_at(&path, |latest| {
            let mut config = latest.clone();
            config.transcription.language = "pt".into();
            Ok(config)
        })
        .unwrap();
        assert_eq!(saved.transcription.models, external.transcription.models);
        assert_eq!(saved.base_url, external.base_url);
        assert_eq!(saved.transcription.language, "pt");
        fs::write(&path, "{ invalid").unwrap();
        assert!(update_config_at(&path, |latest| Ok(latest.clone())).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ invalid");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_edits_are_serialized_and_use_distinct_temporary_files() {
        let dir = temp_dir("concurrent-edits");
        let path = dir.join(CONFIG_FILE);
        assert_ne!(config_temporary_path(&path), config_temporary_path(&path));
        save_config_at(&path, &Config::default()).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    update_config_at(path, |latest| {
                        let mut config = latest.clone();
                        config.transcription.attempt_timeout_seconds += 1;
                        Ok(config)
                    })
                    .unwrap();
                });
            }
        });
        assert_eq!(
            load_config_at(&path)
                .unwrap()
                .transcription
                .attempt_timeout_seconds,
            38
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keys_are_validated_before_storage() {
        assert_eq!(
            validate_key("  sk-or-v1-0123456789abcdef  ").unwrap(),
            "sk-or-v1-0123456789abcdef"
        );
        assert!(validate_key("short").is_err());
        assert!(validate_key("sk-or-v1-0123456789 abcdef").is_err());
        assert!(validate_key("sk-or-v1-0123456789\"abcdef").is_err());
        assert_eq!(key_suffix("sk-or-v1-abcd1234"), "1234");
        assert_eq!(key_suffix("ab"), "ab");
    }

    #[test]
    fn key_description_summarizes_usage() {
        let summary = describe_key(
            br#"{"data":{"label":"sk-or-v1-abc...","usage":1.234,"limit":null,"limit_remaining":null}}"#,
        )
        .unwrap();
        assert_eq!(
            summary,
            "Key works · label sk-or-v1-abc... · used $1.23 · no limit"
        );
        let limited =
            describe_key(br#"{"data":{"usage":0,"limit":10,"limit_remaining":7.5}}"#).unwrap();
        assert_eq!(limited, "Key works · used $0.00 · $7.50 left");
    }

    #[test]
    fn endpoint_joins_without_double_slashes() {
        let config = Config {
            base_url: "https://example.test/api/v1/".into(),
            ..Config::default()
        };
        assert_eq!(
            config.endpoint("audio/transcriptions"),
            "https://example.test/api/v1/audio/transcriptions"
        );
    }

    #[test]
    fn excerpt_is_single_line_and_bounded() {
        let body = format!("line one\n  line two {}", "x".repeat(400));
        let excerpt = excerpt(body.as_bytes());
        assert!(!excerpt.contains('\n'));
        assert!(excerpt.starts_with("line one line two"));
        assert!(excerpt.chars().count() <= 201);
    }
}
