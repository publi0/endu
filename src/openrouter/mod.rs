//! Fork-only OpenRouter runtime: cloud transcription with an ordered model
//! fallback chain and optional LLM cleanup of the transcript.
//!
//! Everything fork-specific lives under this module so upstream merges touch
//! as few shared files as possible. The behavior is compiled in by the
//! `openrouter` Cargo feature (see `FORK.md`); without it the module is inert
//! and HEX keeps its upstream local-model behavior.
//!
//! Configuration is a JSON file in Application Support (`openrouter.json`),
//! re-read on every request so edits apply without restarting. The API key
//! comes from `OPENROUTER_API_KEY`, the config file, or the macOS Keychain
//! (`security add-generic-password -s hex-openrouter -a openrouter -w`).

#[cfg(test)]
mod catalog_tests;
pub mod cleanup;
mod http;
pub mod transcribe;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail};
use serde::{Deserialize, Serialize};

/// True when this binary is the OpenRouter fork build.
pub const ENABLED: bool = cfg!(feature = "openrouter");

/// The fork keeps its settings, logs, and lock apart from an installed
/// upstream HEX (`voice-control`), so the two apps never share state.
pub const SUPPORT_DIR_NAME: Option<&str> = if ENABLED {
    Some("hex-openrouter")
} else {
    None
};

pub const CONFIG_FILE: &str = "openrouter.json";
pub const KEYCHAIN_SERVICE: &str = "hex-openrouter";
pub const KEYCHAIN_ACCOUNT: &str = "openrouter";
pub const API_KEY_ENV: &str = "OPENROUTER_API_KEY";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Config {
    /// Optional plaintext key. Prefer the Keychain; see the module docs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// OpenRouter-compatible API root.
    pub base_url: String,
    pub transcription: TranscriptionConfig,
    pub cleanup: CleanupConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct TranscriptionConfig {
    /// Tried in order. Any failure (transport, timeout, HTTP error, invalid
    /// response) moves on to the next model.
    pub models: Vec<String>,
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
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct CleanupConfig {
    /// Off by default: when on, a text model rewrites each dictation before
    /// Modes processing. Any failure pastes the raw transcript instead.
    pub enabled: bool,
    pub models: Vec<String>,
    pub timeout_seconds: u64,
    /// Replaces the built-in system prompt when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: "https://openrouter.ai/api/v1".into(),
            transcription: TranscriptionConfig::default(),
            cleanup: CleanupConfig::default(),
        }
    }
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            models: vec![
                "openai/whisper-large-v3-turbo".into(),
                "openai/gpt-4o-mini-transcribe".into(),
                "mistralai/voxtral-mini-transcribe".into(),
            ],
            attempt_timeout_seconds: 30,
            total_timeout_seconds: 90,
            chunk_seconds: 120,
            rate_limit_retry_max_wait_ms: 2_000,
            temperature: Some(0.0),
        }
    }
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            models: vec![
                "openai/gpt-4o-mini".into(),
                "google/gemini-2.5-flash".into(),
            ],
            timeout_seconds: 15,
            prompt: None,
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
    load_config_at(&path)
}

pub(crate) fn load_config_at(path: &Path) -> Result<Config> {
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

/// Ready to transcribe: readable config, at least one model, and a key. Used
/// as the "installed" state of the OpenRouter catalog entry.
pub fn is_configured() -> bool {
    load_config().is_ok_and(|config| {
        config
            .transcription
            .models
            .iter()
            .any(|model| !model.trim().is_empty())
            && api_key(&config).is_ok()
    })
}

#[cfg(target_os = "macos")]
fn keychain_key() -> Option<String> {
    // Reading through /usr/bin/security keeps the item's ACL owned by the tool
    // that created it, so no Keychain prompt appears for the app.
    let output = std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let key = String::from_utf8(output.stdout).ok()?;
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
        assert!(config.cleanup.enabled);
        assert_eq!(config.cleanup.models, CleanupConfig::default().models);
        assert_eq!(config.base_url, "https://openrouter.ai/api/v1");
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
