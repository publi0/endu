# Hex Agent Guide

## Purpose

A slim macOS fork of HEX: hold a shortcut to record, trim the silence, send
the clip to OpenRouter with an ordered model fallback chain, and paste the
transcript. Settings, Models, History, and Statistics are the only panes.
OpenRouter key, language, models, and advanced limits belong in Models;
silence trimming belongs in Settings under Microphone. Everything
else from upstream (local models, voice commands, Voice Action, Modes and
OpenCode, meetings, the local API and SDK, Linux) was deleted on purpose; do
not reintroduce seams for them.

## Architecture

- `listener`: the hotkey control loop. It owns the shortcut machine, the
  microphone timeline, and the pipeline, and reports to the HUD and event log.
- `suppression`: the macOS event tap, shortcut suppression, and the
  configurable dictation-hotkey state machine.
- `dictation`: warm pre-roll, growable capture, and 16 kHz resampling.
- `dictation_audio`: the authoritative microphone timeline, lazy stream
  lifecycle, recording owner, exact shortcut boundaries, and a bounded,
  disposable audio projection used only for the HUD meter.
- `audio`: `cpal` device enumeration, timestamped mono PCM, live selection,
  and bounded stream recovery.
- `microphone`: explicit device-bound channel selection and the last clip's
  in-memory RMS/peak diagnostics, measured at 16 kHz before silence trimming.
- `pipeline`: one transcription worker and one ordered output worker with
  bounded queues, cancellation, paste-last, and History recording.
- `openrouter`: configuration (`openrouter.json`), Keychain key handling,
  energy VAD (`vad`), chunking and fallback (`transcribe`), curl transport
  (`http`), the STT catalog (`catalog`), History reports (`report`), daily
  statistics (`stats`), and the Models configuration and Statistics GPUI views.
- `paste`: clipboard insertion, continuation joins, and generation-safe
  clipboard restoration.
- `keyboard`: active-layout key resolution and synthetic Command shortcuts.
- `recording_environment`: idle-sleep prevention, output muting, and media
  pause while a hold is intentional.
- `volume_fade`: interruptible output fades with guarded restoration; manual
  volume or mute changes relinquish ownership when detected.
- `context`: the foreground application name, recorded in History.
- `history`: the owner-only bounded store of pasted text and metadata.
- `events`: bounded asynchronous NDJSON observations in `logs/live.ndjson`.
- `app_settings`: persisted app settings and their live runtime projection.
- `app_window`, `desktop_ui`, `text_input`: the GPUI window and controls.
- `desktop`: the GPUI application, menus, menu bar item, HUD, and listener
  thread lifetime.
- `dictation_indicator`: the click-through Metal HUD.
- `status_item`, `login_item`, `onboarding`, `permission_guide`, `instance`,
  `feedback`: menu bar, launch at login, setup gate, permission helper, single
  instance lock, and tones.

## Invariants

- Capture never waits on transcription, paste, History, or statistics. Queues
  stay bounded. Starting a new capture never cancels accepted work, and output
  keeps submission order.
- Hold to dictate, release to transcribe. Captures shorter than 300 ms
  discard. A 450 ms pre-roll protects speech onset. A second tap within 300 ms
  locks; the next press finishes and Escape cancels. Escape with no capture
  cancels the newest unfinished job; cancelled jobs never paste.
- Recording audio behavior and idle-sleep prevention begin only after the
  intentional-hold threshold.
- Preserve the existing channel mix until the user explicitly selects a
  channel. Bind that choice to the device UID, apply changes between clips,
  and fall back to the mix with a visible warning if the channel disappears.
  Mono samples must stay unchanged. Never infer a voice channel from volume.
- RMS/peak diagnostics never modify or retain audio. Do not enable automatic
  normalization, AGC, high-pass filtering, Apple VoiceProcessing, or neural
  denoising by default without a separate user decision.
- Output fades run on the environment worker, never on the capture callback
  or shortcut loop. Preserve the original volume across quick restarts and
  restore only when Hex still owns the level; do not overwrite detected
  manual volume/mute changes.
- CoreAudio capture timestamps and annotated-session CGEvent timestamps share
  the boot-time nanosecond clock; do not apply the Mach timebase to them.
- GUI startup calls `keyboard::initialize_layout` on the main thread before
  anything else uses the keyboard.
- Release dictation starts only after Microphone, Input Monitoring,
  Accessibility, and an OpenRouter key are ready.
- `Release microphone while idle` opens the device on the shortcut with no
  pre-roll and closes it once capture is idle.
- The API key never reaches argv, logs, or temporary files: `security -i`
  reads it from stdin and curl reads its config from stdin. Resolution order is
  `OPENROUTER_API_KEY`, then `api_key` in the file, then the Keychain.
- `openrouter.json` is re-read on every dictation. Any model error falls back
  to the next model; a 429 asking to wait at most the configured time is
  retried once on the same model.
- Statistics record daily totals only, never text or audio, and recording
  them must never affect dictation.
- History records only successful pasted output: text plus bounded metadata,
  never audio. Retention defaults to seven days with hard entry and byte caps.
- Audio is never persisted.
- The HUD is observational and click-through.
- Every pane renders the `desktop_ui` scaffold: `pane_header` or
  `pane_header_with_action`, then one column bounded by `PANE_CONTENT_WIDTH`.

## Mandatory local validation before every commit

**All validation runs locally, on macOS, before committing. GitHub Actions
is for releases only.** This is a project requirement, not a suggestion.

Before creating **any development commit**, run the following on the final working tree:

```sh
scripts/check-local.sh
```

The script must finish successfully. It runs:

- `cargo fmt --all --check`
- `cargo clippy --locked --all-targets -- -D warnings`
- `cargo test --locked --timings` (the full unit and integration suite)
- Shell syntax checks for the build and cask scripts.
- `python3 -m unittest discover -s scripts -p 'test_release.py' -v`
- `git diff --check`

The complete suite runs once in the development profile. Use
`scripts/check-local.sh --release` when changing release optimization,
mode-dependent behavior, or investigating a release-only regression; that
also runs the suite with `--release` locally before the commit. Do not add a
second optimized build to every ordinary commit without a concrete reason.

Keep the Rust toolchain, Cargo cache, and `target/` directory stable between
runs. Do not run `cargo clean` as routine validation. The script reports time
per step, and Cargo writes build timings under `target/cargo-timings/`.

Fix failures locally before committing or pushing. Do not commit first and
use GitHub to discover formatting, lint, compilation, or test failures. Do
not treat a successful release build as evidence that tests passed. Record
the local command and result when handing off work.

The script isolates application data in a temporary directory and removes
the API-key environment variable for the test process. Do not point tests at
the user's real settings, History, Statistics, or credentials. Native tests
that are explicitly ignored remain manual checks; report that limitation
rather than claiming they ran.

Use macOS with Rust stable, Python 3.11 or newer, and Xcode with the Metal
compiler. Portable tests on Linux do not replace the required macOS checks.
If the required environment is unavailable, explain the blocker and leave
the changes uncommitted. Do not move the checks into GitHub Actions.

## Build and preview

```sh
scripts/build-app.sh  # target/app/Hex-<version>.zip
cargo run -- preview settings
cargo run -- preview models
cargo run -- preview history --open-history-retention
cargo run -- preview statistics
cargo run -- preview onboarding
cargo run -- preview dictation-hud
```

Only macOS builds the app. Other platforms can run the portable modules'
tests, but there is no Linux app. Previews must not access real configuration,
credentials, or network services.

## GitHub: releases only

`.github/workflows/release.yml` may build, package, sign, publish a release,
and update `Casks/hex-openrouter.rb`. **Do not add formatting, linting, unit
tests, integration tests, or other validation jobs to GitHub Actions.** Do
not call `scripts/check-local.sh` or any test suite from a workflow. All of
those steps belong to the local pre-commit process above.

The package version in `Cargo.toml` is the release version. Keep its package
entry in `Cargo.lock` in sync. Publish only when that version is not already
published: `3.0.0` becomes tag `v3.0.0`, title `Hex 3.0.0`, and asset
`Hex-3.0.0.zip`. Never append a run number, a `fork-` prefix, or automatically
increment the version. Future releases require an explicit version change.

The release bot may create a generated cask-only commit as part of
publication. That commit must not change application code or run checks in
Actions; development changes still require the local checks above.

Published versions are immutable. Reruns may finish an interrupted release
or repair the cask, but must not overwrite a published asset, duplicate the
release, or downgrade the cask/latest release. Update the cask only after its
release asset is available and verified. Keep `main` installable.

The app is named **Hex**. Keep the existing bundle id, Keychain identifiers,
data directory, and `hex-openrouter` Homebrew token for upgrade continuity.

## Diagnostics

- `~/Library/Application Support/hex-openrouter/logs/live.ndjson`: state,
  dictation phases, and foreground context.
- `~/Library/Application Support/hex-openrouter/logs/process.log`: Rust,
  CoreAudio, and OpenRouter diagnostics.
