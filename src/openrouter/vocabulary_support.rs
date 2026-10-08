//! Discover routes and verify hint parameter contracts, never infer support from HTTP 200.

use std::collections::BTreeMap;
use std::fs;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Config, http};

const SCHEMA: u32 = 2;
const TIMEOUT: Duration = Duration::from_secs(15);
const VERIFIED_TTL: u64 = 7 * 24 * 60 * 60;
static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));
static SAVE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum Adapter {
    AzurePhrases,
    OpenAiKeywords,
    OpenAiPrompt,
    GroqPrompt,
    DeepgramKeyterm,
}

impl Adapter {
    fn provider(self) -> &'static str {
        match self {
            Self::AzurePhrases => "azure",
            Self::OpenAiKeywords | Self::OpenAiPrompt => "openai",
            Self::GroqPrompt => "groq",
            Self::DeepgramKeyterm => "deepgram",
        }
    }
    fn field(self) -> &'static str {
        match self {
            Self::AzurePhrases => "phrases",
            Self::OpenAiKeywords => "keywords",
            Self::OpenAiPrompt | Self::GroqPrompt => "prompt",
            Self::DeepgramKeyterm => "keyterm",
        }
    }
    fn limits(self) -> (usize, usize) {
        match self {
            Self::AzurePhrases => (2_000, 256_000),
            Self::GroqPrompt | Self::OpenAiPrompt => (50, 200),
            Self::OpenAiKeywords | Self::DeepgramKeyterm => (50, 400),
        }
    }
    fn options(self, terms: &[String], invalid: bool) -> Value {
        let value = if invalid {
            json!({"hex_invalid_parameter_type": true})
        } else if matches!(self, Self::GroqPrompt | Self::OpenAiPrompt) {
            json!(terms.join(", "))
        } else {
            json!(terms)
        };
        match self {
            Self::AzurePhrases => json!({"phraseList": {"phrases": value}}),
            _ => json!({self.field(): value}),
        }
    }
}

fn adapters(provider: &str) -> &'static [Adapter] {
    match provider {
        "azure" => &[Adapter::AzurePhrases],
        "openai" => &[Adapter::OpenAiKeywords, Adapter::OpenAiPrompt],
        "groq" => &[Adapter::GroqPrompt],
        "deepgram" => &[Adapter::DeepgramKeyterm],
        _ => &[],
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum Verification {
    Verified,
    Observed,
    Ignored,
    Rejected,
    Unavailable,
    Unknown,
}

impl Verification {
    fn label(self) -> &'static str {
        match self {
            Self::Verified => "Hint parameter verified",
            Self::Observed => "Vocabulary effect observed in test",
            Self::Ignored => "Vocabulary effect unverified · local only",
            Self::Rejected => "Parameter unsupported · local only",
            Self::Unavailable => "Could not verify · local only",
            Self::Unknown => "Unknown provider · local only",
        }
    }
    fn ttl(self) -> u64 {
        match self {
            Self::Verified | Self::Observed => VERIFIED_TTL,
            Self::Unavailable => 300,
            _ => 24 * 60 * 60,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Entry {
    schema: u32,
    base: String,
    model: String,
    fingerprint: String,
    adapter: Adapter,
    checked: u64,
    verification: Verification,
}

impl Entry {
    fn fresh(&self, now: u64) -> bool {
        self.schema == SCHEMA && now >= self.checked && now - self.checked < self.verification.ttl()
    }
}

#[derive(Default)]
struct State {
    loaded: bool,
    entries: Vec<Entry>,
    rows: Vec<(String, String)>,
    running: bool,
    last_started: u64,
    last_configuration: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn load(state: &mut State) {
    if state.loaded {
        return;
    }
    state.loaded = true;
    if let Ok(path) = cache_path()
        && let Ok(data) = fs::read(path)
        && data.len() <= 512 * 1024
        && let Ok(entries) = serde_json::from_slice::<Vec<Entry>>(&data)
    {
        state.entries = entries
            .into_iter()
            .filter(|entry| entry.schema == SCHEMA)
            .take(256)
            .collect();
    }
}

fn cache_path() -> color_eyre::Result<std::path::PathBuf> {
    Ok(crate::app_paths::support_dir()?.join("vocabulary-support.json"))
}

fn save() {
    let _file = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let entries = STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .clone();
    let persist = || -> color_eyre::Result<()> {
        use std::io::Write;
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let path = cache_path()?;
        fs::create_dir_all(path.parent().expect("support directory"))?;
        let temporary = path.with_extension("tmp");
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec(&entries)?)?;
        fs::rename(temporary, path)?;
        Ok(())
    };
    if persist().is_err() {
        tracing::warn!("could not persist vocabulary support cache");
    }
}

pub fn status() -> Vec<(String, String)> {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).rows.clone()
}

pub fn schedule(force: bool) {
    #[cfg(target_os = "macos")]
    if crate::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Ok(mut config) = super::load_config() else {
        return;
    };
    config.transcription.models.retain(|id| {
        crate::providers::ModelRef::parse(id).provider == crate::providers::Provider::OpenRouter
    });
    if config.transcription.models.is_empty() {
        return;
    }
    let signature = format!("{}:{:?}", config.base_url, config.transcription.models);
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let elapsed = now().saturating_sub(state.last_started);
    let cached = config.transcription.models.iter().all(|model| {
        state.entries.iter().any(|entry| {
            entry.model == *model && entry.base == config.base_url && entry.fresh(now())
        })
    });
    if state.running
        || (!force
            && state.last_configuration == signature
            && (elapsed < 300 || (cached && elapsed < 3600)))
    {
        return;
    }
    state.running = true;
    state.last_started = now();
    state.last_configuration = signature;
    drop(state);
    std::thread::spawn(move || {
        validate(&config, force);
        STATE.lock().unwrap_or_else(|e| e.into_inner()).running = false;
        // A model selection may have changed while the worker was checking.
        schedule(false);
    });
}

#[derive(Deserialize)]
struct Listing {
    data: ModelEndpoints,
}
#[derive(Deserialize)]
struct ModelEndpoints {
    id: String,
    endpoints: Vec<Endpoint>,
}
#[derive(Deserialize)]
struct Endpoint {
    tag: String,
    name: String,
}

fn endpoint_url(config: &Config, model: &str) -> Option<String> {
    let (owner, name) = model.split_once('/')?;
    if owner.is_empty()
        || name.is_empty()
        || model.len() > 180
        || !model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ':'))
        || name.contains('/')
        || owner == "."
        || owner == ".."
        || name == "."
        || name == ".."
    {
        return None;
    }
    Some(config.endpoint(&format!("models/{owner}/{name}/endpoints")))
}

fn official(config: &Config) -> bool {
    config.base_url.trim_end_matches('/') == "https://openrouter.ai/api/v1"
}

/// Read only cached evidence for this endpoint/model; never starts a probe.
pub fn has_verified_support(config: &Config, model: &str) -> bool {
    if !official(config)
        || crate::providers::ModelRef::parse(model).provider
            != crate::providers::Provider::OpenRouter
    {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    load(&mut state);
    state.entries.iter().any(|entry| {
        entry.base == config.base_url
            && entry.model == model
            && entry.fresh(now())
            && matches!(
                entry.verification,
                Verification::Verified | Verification::Observed
            )
    })
}

pub fn validate(config: &Config, force: bool) -> Vec<(String, String)> {
    let models: Vec<_> = config
        .transcription
        .models
        .iter()
        .filter(|id| {
            crate::providers::ModelRef::parse(id).provider == crate::providers::Provider::OpenRouter
        })
        .take(16)
        .cloned()
        .collect();
    {
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        load(&mut state);
        state.rows = models
            .iter()
            .map(|model| (model.clone(), "Checking model support…".into()))
            .collect();
    }
    if models.is_empty() {
        return status();
    }
    // Fresh cached evidence needs no key; reading it eagerly would show a
    // Keychain prompt at launch after every update.
    let mut key: Option<Option<String>> = None;
    for (index, model) in models.iter().enumerate() {
        #[cfg(target_os = "macos")]
        if crate::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let mut descriptions = Vec::new();
        let endpoints = if official(config) {
            endpoint_url(config, model)
                .and_then(|url| http::get(&url, "", TIMEOUT).ok())
                .filter(|response| response.is_success() && response.body.len() <= 512 * 1024)
                .and_then(|response| serde_json::from_slice::<Listing>(&response.body).ok())
                .filter(|listing| listing.data.id == *model)
                .map(|listing| listing.data.endpoints)
        } else {
            None
        };
        if let Some(endpoints) = endpoints {
            if endpoints.len() > 1 {
                // Transcription routing cannot pin a provider. Without response
                // attribution, two probes could hit different providers.
                let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
                state
                    .entries
                    .retain(|entry| entry.model != *model || entry.base != config.base_url);
                state.rows[index].1 =
                    "Multiple routes · verification cannot isolate a provider · local only".into();
                continue;
            }
            // Discovery is authoritative: never retain a provider/version that vanished.
            let fingerprints: Vec<_> = endpoints
                .iter()
                .map(|endpoint| format!("{}:{}", endpoint.tag, endpoint.name))
                .collect();
            STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entries
                .retain(|entry| {
                    entry.model != *model
                        || entry.base != config.base_url
                        || fingerprints.contains(&entry.fingerprint)
                });
            for endpoint in endpoints.into_iter().take(8) {
                let fingerprint = format!("{}:{}", endpoint.tag, endpoint.name);
                let mut result = Verification::Unknown;
                for &adapter in adapters(&endpoint.tag) {
                    let existing = STATE
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .entries
                        .iter()
                        .find(|entry| {
                            entry.base == config.base_url
                                && entry.model == *model
                                && entry.fingerprint == fingerprint
                                && entry.adapter == adapter
                                && entry.fresh(now())
                        })
                        .cloned();
                    let checked = if !force {
                        existing.as_ref().map_or_else(now, |entry| entry.checked)
                    } else {
                        now()
                    };
                    result = if !force && let Some(entry) = existing {
                        entry.verification
                    } else if let Some(key) = key
                        .get_or_insert_with(|| {
                            official(config)
                                .then(|| super::api_key(config).ok())
                                .flatten()
                        })
                        .as_ref()
                    {
                        let audio = super::transcribe::encode_base64(include_bytes!(
                            "../../resources/vocabulary-probe.wav"
                        ));
                        verify_adapter(adapter, |options| {
                            let body = json!({"model": model, "temperature": 0, "input_audio": {"data": audio, "format": "wav"},
                                "provider": {"options": {adapter.provider(): options}}});
                            let response = http::post_json(
                                &config.endpoint("audio/transcriptions"),
                                key,
                                &body.to_string(),
                                TIMEOUT,
                            )
                            .ok();
                            tracing::debug!(
                                model,
                                provider = adapter.provider(),
                                field = adapter.field(),
                                status = response.as_ref().map(|r| r.status),
                                field_mentioned =
                                    response.as_ref().is_some_and(|r| String::from_utf8_lossy(
                                        &r.body
                                    )
                                    .to_lowercase()
                                    .contains(adapter.field())),
                                "synthetic vocabulary probe completed"
                            );
                            response
                        })
                    } else {
                        Verification::Unavailable
                    };
                    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
                    state.entries.retain(|entry| {
                        !(entry.base == config.base_url
                            && entry.model == *model
                            && entry.adapter == adapter)
                    });
                    state.entries.push(Entry {
                        schema: SCHEMA,
                        base: config.base_url.clone(),
                        model: model.clone(),
                        fingerprint: fingerprint.clone(),
                        adapter,
                        checked,
                        verification: result,
                    });
                    if matches!(
                        result,
                        Verification::Verified | Verification::Observed | Verification::Unavailable
                    ) {
                        break;
                    }
                }
                descriptions.push(format!("{}: {}", endpoint.tag, result.label()));
            }
        }
        if descriptions.is_empty() {
            let cached = STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entries
                .iter()
                .any(|entry| {
                    entry.base == config.base_url
                        && entry.model == *model
                        && entry.fresh(now())
                        && matches!(
                            entry.verification,
                            Verification::Verified | Verification::Observed
                        )
                });
            descriptions.push(
                if cached {
                    "Using cached verification; route refresh unavailable"
                } else {
                    "Support unverified · local only"
                }
                .into(),
            );
        }
        STATE.lock().unwrap_or_else(|e| e.into_inner()).rows[index].1 = descriptions.join("; ");
    }
    save();
    status()
}

fn verify_adapter(
    adapter: Adapter,
    mut request: impl FnMut(Value) -> Option<http::Response>,
) -> Verification {
    let names = vec!["Zephyr-Files".to_string()];
    let Some(valid) = request(adapter.options(&names, false)) else {
        return Verification::Unavailable;
    };
    if !valid.is_success() {
        return if matches!(valid.status, 400 | 422) {
            Verification::Rejected
        } else {
            Verification::Unavailable
        };
    }
    if serde_json::from_slice::<Value>(&valid.body)
        .ok()
        .and_then(|value| value.get("text").cloned())
        .is_none_or(|value| !value.is_string())
    {
        return Verification::Unavailable;
    }
    let Some(invalid) = request(adapter.options(&names, true)) else {
        return Verification::Unavailable;
    };
    if contract_rejection(&invalid, adapter.field()) {
        return Verification::Verified;
    }
    // Lenient APIs may drop malformed values while honoring valid ones. A 200
    // does not prove either support or lack of support. Look for a reproducible
    // A/B/A spelling effect using only the fixed synthetic speech fixture.
    if !invalid.is_success() && !matches!(invalid.status, 400 | 422) {
        return Verification::Unavailable;
    }
    if !probe_spelling(&valid, "zephyrfiles", "xephyrfiles") {
        return if invalid.is_success() {
            Verification::Ignored
        } else {
            Verification::Unavailable
        };
    }
    let alternate = vec!["Xephyr-Files".to_string()];
    let Some(changed) = request(adapter.options(&alternate, false)) else {
        return Verification::Unavailable;
    };
    if !probe_spelling(&changed, "xephyrfiles", "zephyrfiles") {
        return Verification::Ignored;
    }
    let Some(repeated) = request(adapter.options(&names, false)) else {
        return Verification::Unavailable;
    };
    if probe_spelling(&repeated, "zephyrfiles", "xephyrfiles") {
        Verification::Observed
    } else {
        Verification::Ignored
    }
}

fn probe_spelling(response: &http::Response, expected: &str, excluded: &str) -> bool {
    if !response.is_success() {
        return false;
    }
    let Some(text) = serde_json::from_slice::<Value>(&response.body)
        .ok()
        .and_then(|body| body.get("text").and_then(Value::as_str).map(str::to_owned))
    else {
        return false;
    };
    let normalized: String = text
        .chars()
        .filter(char::is_ascii_alphabetic)
        .flat_map(char::to_lowercase)
        .collect();
    normalized.contains(expected) && !normalized.contains(excluded)
}

fn contract_rejection(response: &http::Response, field: &str) -> bool {
    if !matches!(response.status, 400 | 422) {
        return false;
    }
    let body = String::from_utf8_lossy(&response.body).to_lowercase();
    body.contains(field)
        && [
            "type", "array", "list", "string", "invalid", "expected", "must be",
        ]
        .iter()
        .any(|word| body.contains(word))
        && !body.contains("unknown parameter")
        && !body.contains("unrecognized parameter")
        && !body.contains("unsupported parameter")
}

#[derive(Clone, Default)]
pub struct HintPlan {
    models: BTreeMap<String, Value>,
}

impl HintPlan {
    pub fn cached(config: &Config, vocabulary: &crate::vocabulary::Snapshot) -> Self {
        if !vocabulary.settings().remote_hints
            || vocabulary.settings().terms.is_empty()
            || !official(config)
        {
            return Self::default();
        }
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        load(&mut state);
        Self::from_entries(config, vocabulary, &state.entries, now())
    }

    fn from_entries(
        config: &Config,
        vocabulary: &crate::vocabulary::Snapshot,
        entries: &[Entry],
        at: u64,
    ) -> Self {
        let mut models: BTreeMap<String, Value> = BTreeMap::new();
        for entry in entries {
            if entry.base == config.base_url
                && config.transcription.models.contains(&entry.model)
                && matches!(
                    entry.verification,
                    Verification::Verified | Verification::Observed
                )
                && entry.fresh(at)
            {
                let (max_terms, max_bytes) = entry.adapter.limits();
                let terms = vocabulary.remote_terms(max_terms, max_bytes);
                let options = models
                    .entry(entry.model.clone())
                    .or_insert_with(|| json!({"options": {}}));
                options["options"][entry.adapter.provider()] = entry.adapter.options(&terms, false);
            }
        }
        Self { models }
    }
    #[cfg(test)]
    pub(crate) fn fixture(model: &str, options: Value) -> Self {
        Self {
            models: BTreeMap::from([(model.into(), options)]),
        }
    }
    pub fn for_model(&self, model: &str) -> Option<&Value> {
        self.models.get(model)
    }
    pub fn rejected(&self, model: &str, response: &http::Response) -> bool {
        if self.for_model(model).is_none() || !matches!(response.status, 400 | 422) {
            return false;
        }
        let body = String::from_utf8_lossy(&response.body).to_lowercase();
        ["phraselist", "phrases", "keywords", "keyterm", "prompt"]
            .iter()
            .any(|field| body.contains(field))
    }
    pub fn invalidate(config: &Config, model: &str) {
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        for entry in &mut state.entries {
            if entry.model == model && entry.base == config.base_url {
                entry.verification = Verification::Rejected;
                entry.checked = now();
            }
        }
        for (id, label) in &mut state.rows {
            if id == model {
                *label = "Hints rejected · local only until rechecked".into();
            }
        }
        drop(state);
        save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response(status: u16, body: &str) -> http::Response {
        http::Response {
            status,
            body: body.as_bytes().to_vec(),
            retry_after: None,
        }
    }
    #[test]
    fn successful_status_alone_is_not_support_and_unrelated_errors_are_not_evidence() {
        assert_eq!(
            verify_adapter(Adapter::AzurePhrases, |_| Some(response(
                200,
                r#"{"text":"Hello."}"#
            ))),
            Verification::Ignored
        );
        let mut calls = 0;
        assert_eq!(
            verify_adapter(Adapter::AzurePhrases, |_| {
                calls += 1;
                Some(if calls == 1 {
                    response(200, r#"{"text":"Hello."}"#)
                } else {
                    response(400, "invalid audio")
                })
            }),
            Verification::Unavailable
        );
        assert!(!contract_rejection(
            &response(400, "unsupported parameter keywords"),
            "keywords"
        ));
    }
    #[test]
    fn matching_negative_control_proves_contract_and_never_contains_user_names() {
        let mut calls = 0;
        let outcome = verify_adapter(Adapter::AzurePhrases, |options| {
            calls += 1;
            if calls == 1 {
                assert_eq!(options["phraseList"]["phrases"], json!(["Zephyr-Files"]));
                Some(response(200, r#"{"text":"Hello."}"#))
            } else {
                assert!(options["phraseList"]["phrases"].is_object());
                Some(response(422, "phraseList.phrases must be an array"))
            }
        });
        assert_eq!(outcome, Verification::Verified);
        assert_eq!(calls, 2);
    }
    #[test]
    fn lenient_parameters_require_a_reproducible_spelling_effect() {
        let mut calls = 0;
        let observed = verify_adapter(Adapter::DeepgramKeyterm, |_| {
            let text = match calls {
                0 | 3 => "We use Zephyr files",
                1 => "We use Zephyr files",
                _ => "We use Xephyr files",
            };
            calls += 1;
            Some(response(200, &json!({"text": text}).to_string()))
        });
        assert_eq!(observed, Verification::Observed);
        assert_eq!(calls, 4);
        assert_eq!(
            verify_adapter(Adapter::DeepgramKeyterm, |_| Some(response(
                200,
                r#"{"text":"We use Zephyr files"}"#
            ))),
            Verification::Ignored
        );
        assert_eq!(
            verify_adapter(Adapter::AzurePhrases, |_| Some(response(200, "{}"))),
            Verification::Unavailable
        );
    }

    #[test]
    fn cache_requires_current_schema_clock_and_ttl_and_urls_are_bounded() {
        let mut entry = Entry {
            schema: SCHEMA,
            base: String::new(),
            model: String::new(),
            fingerprint: String::new(),
            adapter: Adapter::AzurePhrases,
            checked: 100,
            verification: Verification::Verified,
        };
        assert!(entry.fresh(101));
        assert!(!entry.fresh(99));
        assert!(!entry.fresh(100 + VERIFIED_TTL));
        entry.schema += 1;
        assert!(!entry.fresh(101));
        let config = Config::default();
        assert!(endpoint_url(&config, "microsoft/mai-transcribe-2").is_some());
        for model in ["../secret", "x/../../secret", "x/y?key=secret", "x/"] {
            assert!(endpoint_url(&config, model).is_none());
        }
    }
    #[test]
    fn options_are_scoped_to_provider_and_unknown_providers_stay_local() {
        let names = vec!["nimbus-files".into()];
        assert_eq!(
            Adapter::AzurePhrases.options(&names, false),
            json!({"phraseList": {"phrases": ["nimbus-files"]}})
        );
        assert_eq!(
            Adapter::OpenAiKeywords.options(&names, false),
            json!({"keywords": ["nimbus-files"]})
        );
        assert!(adapters("new-provider").is_empty());
    }

    #[test]
    fn documented_model_metadata_does_not_bypass_fresh_route_evidence() {
        const MODEL: &str = "microsoft/mai-transcribe-2";
        let mut config = Config::default();
        config.transcription.models = vec![MODEL.into()];
        let vocabulary = crate::vocabulary::Snapshot::new(crate::vocabulary::Vocabulary {
            terms: vec!["Synthetic Nimbus".into()],
            ..crate::vocabulary::Vocabulary::default()
        });
        assert!(
            crate::providers::ModelRef::parse(MODEL)
                .capabilities()
                .keywords
        );
        assert!(
            HintPlan::from_entries(&config, &vocabulary, &[], 100)
                .for_model(MODEL)
                .is_none()
        );
        let mut entry = Entry {
            schema: SCHEMA,
            base: config.base_url.clone(),
            model: MODEL.into(),
            fingerprint: "synthetic-single-route".into(),
            adapter: Adapter::AzurePhrases,
            checked: 99,
            verification: Verification::Unknown,
        };
        for verification in [
            Verification::Unknown,
            Verification::Unavailable,
            Verification::Ignored,
            Verification::Rejected,
        ] {
            entry.verification = verification;
            assert!(
                HintPlan::from_entries(&config, &vocabulary, std::slice::from_ref(&entry), 100)
                    .for_model(MODEL)
                    .is_none()
            );
        }
        entry.verification = Verification::Verified;
        assert!(
            HintPlan::from_entries(&config, &vocabulary, std::slice::from_ref(&entry), 100)
                .for_model(MODEL)
                .is_some()
        );
        assert!(
            HintPlan::from_entries(&config, &vocabulary, &[entry], 99 + VERIFIED_TTL)
                .for_model(MODEL)
                .is_none()
        );
    }
}
