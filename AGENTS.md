# Hex OpenRouter Agent Guide

## Purpose

A slim macOS fork of HEX: hold a shortcut to record, trim the silence, send
the clip to OpenRouter with an ordered model fallback chain, and paste the
transcript. Settings, History, and Statistics are the only windows. Everything
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
- `pipeline`: one transcription worker and one ordered output worker with
  bounded queues, cancellation, paste-last, and History recording.
- `openrouter`: configuration (`openrouter.json`), Keychain key handling,
  energy VAD (`vad`), chunking and fallback (`transcribe`), curl transport
  (`http`), the STT catalog (`catalog`), History reports (`report`), daily
  statistics (`stats`), and the Settings and Statistics GPUI views.
- `paste`: clipboard insertion, continuation joins, and generation-safe
  clipboard restoration.
- `keyboard`: active-layout key resolution and synthetic Command shortcuts.
- `recording_environment`: idle-sleep prevention, output muting, and media
  pause while a hold is intentional.
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

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
fork/build-app.sh
cargo run -- preview settings
cargo run -- preview history --open-history-retention
cargo run -- preview statistics
cargo run -- preview onboarding
cargo run -- preview dictation-hud
```

Only macOS builds the app. On other platforms the crate compiles the portable
modules (`openrouter`, `history`, `events`, `app_paths`) so their tests run.

`fork-check.yml` runs fmt, clippy, and tests on macOS for every branch other
than `main`. Merge to `main` only when it is green: every push to `main` runs
`fork-release.yml`, which publishes `fork-v<version>-<run>` and rewrites
`Casks/hex-openrouter.rb`.

## Diagnostics

- `~/Library/Application Support/hex-openrouter/logs/live.ndjson`: state,
  dictation phases, and foreground context.
- `~/Library/Application Support/hex-openrouter/logs/process.log`: Rust,
  CoreAudio, and OpenRouter diagnostics.
