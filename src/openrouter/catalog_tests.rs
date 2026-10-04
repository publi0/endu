//! How the OpenRouter entry fits HEX's model catalog in both build modes.
//! Runs in the upstream suite (`cargo test`) and in the fork build
//! (`cargo test --features openrouter openrouter::`).

use super::ENABLED;
use crate::transcription_models::{
    AUTO_LANGUAGE, MODELS, ModelRuntime, TranscriptionModelId, TranscriptionSelection,
    choices_for_runtime, definition, validate,
};

#[test]
fn openrouter_has_a_stable_wire_name() {
    let id = TranscriptionModelId::OpenRouter;
    assert_eq!(id.as_str(), "openrouter");
    assert_eq!("openrouter".parse::<TranscriptionModelId>().unwrap(), id);
    assert_eq!(serde_json::to_value(id).unwrap(), "openrouter");
    let model = definition(id);
    assert!(matches!(model.runtime, ModelRuntime::OpenRouter));
    assert_eq!(model.download_bytes(), None);
    assert!(model.supports_language("pt") && model.supports_language(AUTO_LANGUAGE));
}

#[test]
fn catalog_matches_the_build_mode() {
    let openrouter = definition(TranscriptionModelId::OpenRouter);
    assert_eq!(openrouter.available(), ENABLED);
    let local = TranscriptionSelection {
        model: TranscriptionModelId::ParakeetV3,
        language: "pt".into(),
        ..TranscriptionSelection::default()
    };
    let remote = TranscriptionSelection {
        model: TranscriptionModelId::OpenRouter,
        language: AUTO_LANGUAGE.into(),
        ..TranscriptionSelection::default()
    };
    // Local models stay available in both builds.
    assert!(validate(&local).is_ok());
    if ENABLED {
        let default = TranscriptionSelection::default();
        assert_eq!(default.model, TranscriptionModelId::OpenRouter);
        assert_eq!(default.language, AUTO_LANGUAGE);
        assert!(validate(&default).is_ok());
        for language in ["en", "pt", AUTO_LANGUAGE, "zh"] {
            let choices = choices_for_runtime(language);
            assert_eq!(
                choices[0].model.id,
                TranscriptionModelId::OpenRouter,
                "{language}"
            );
            assert!(choices.len() > 1, "local models remain: {language}");
        }
        assert!(validate(&remote).is_ok());
    } else {
        assert_ne!(
            TranscriptionSelection::default().model,
            TranscriptionModelId::OpenRouter
        );
        assert!(
            MODELS
                .iter()
                .filter(|model| model.available())
                .all(|model| model.id != TranscriptionModelId::OpenRouter)
        );
        for language in ["en", "pt", AUTO_LANGUAGE, "zh"] {
            assert!(
                choices_for_runtime(language)
                    .iter()
                    .all(|choice| choice.model.id != TranscriptionModelId::OpenRouter)
            );
        }
        assert!(validate(&remote).is_err());
    }
}
