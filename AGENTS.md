# Endu Agent Guide

The code, its tests and its doc comments are the source of truth. This file
holds only what they cannot tell you: scope, rules that span modules, and
process. Constants, thresholds and provider protocols live in the code.

## Purpose

Endu (Tupi for "to hear") began as a slim macOS fork of Hex. Tap a shortcut to
lock recording, or hold and release; Endu trims silence, sends the clip through
an ordered chain of speech providers and pastes the transcript.

Panes: Settings, Microphone, Providers, Models, Post-processing, HUD, History,
Statistics. Providers holds keys, request limits and the API URL (the last two
in its collapsed Advanced section). Models holds the primary/fallback chain,
per-model options and shared keywords. Microphone holds device, channel,
priority, levels, capture mode, silence trimming and audio behaviour while
dictating. HUD holds indicator position and palettes.

Deleted on purpose; do not reintroduce them or seams for them: local models,
voice commands, Voice Action, Modes and OpenCode, meetings, the local API and
SDK, Linux, and the native Microsoft provider.

## Architecture

Only modules whose name does not say enough:

- `listener`: the hotkey control loop; owns the shortcut machine, the
  microphone timeline and the pipeline.
- `suppression`: the macOS event tap and the dictation-hotkey state machine.
- `dictation_audio`: the authoritative microphone timeline, stream lifecycle
  and exact shortcut boundaries. Its HUD meter projection is disposable.
- `providers`: identities, per-model capabilities, Keychain credentials
  (`keys`), uploads (`batch`, `http_pool`, `opus`), WebSocket sessions
  (`streaming`) and dated list prices (`pricing`).
- `openrouter`: despite the name, shared by every provider: `openrouter.json`,
  VAD, chunking and fallback (`transcribe`), the STT catalog, History reports,
  daily statistics (`stats`), and the Models and Statistics views.
- `pipeline`: one transcription worker and one ordered output worker.
- `doorbell`: the level-triggered wake-up that idle loops block on.
- `recording_environment`, `volume_fade`: sleep prevention, output fades and
  media pause during an intentional hold.
- `context`: the foreground application captured at recording start.
- `recording_recovery`: private clip persistence and the isolated Retry worker.
- `desktop`: the GPUI app, menus, menu bar item, HUD and listener lifetime.
- `dictation_indicator`, `overlay_visibility`: the click-through Metal HUD and
  keeping overlays on the active Space.

## Invariants

### Threads and loops

- Capture never waits on transcription, paste, History or statistics. Queues
  stay bounded. The capture callback only uses bounded `try_send`; credentials,
  DNS, TLS, encoding, socket I/O, statistics I/O and output fades stay off the
  capture and shortcut threads.
- Idle loops block instead of polling, and tick only while they have work.
  Whatever queues work for a sleeping loop rings its `doorbell`. Nothing
  requests UI frames while the window is at rest.
- CoreAudio capture and CGEvent timestamps share the boot-time nanosecond
  clock; do not apply the Mach timebase. Boundaries come from event
  timestamps, never from loop timing.
- GUI startup calls `keyboard::initialize_layout` on the main thread before
  anything else uses the keyboard.

### Shortcut and capture

- One threshold, `dictation::MINIMUM_HOLD_DURATION`, measured from the
  shortcut edge (never the tail), separates tap from hold and discards short
  captures. Default TapOrHold locks on a clean release before it and the next
  press finishes; a longer hold finishes on release. Hold and DoubleTap remain
  selectable legacy modes; DoubleTap uses the sensitivity window in
  `interaction_settings`. A pre-roll protects speech onset while the
  microphone is kept ready.
- Every accepted finish keeps recording for the `Keep recording after
  release` setting (`app_settings::ReleaseTail`). The HUD stays Recording; the
  stop cue, environment restoration and idle microphone release follow the
  tail. A gap, disconnect or quit during the tail finishes the clip (quit: to
  recovery) instead of failing it; a later shortcut edge seals the clip at
  that edge, never cancelling it.
- Enter-to-submit (off by default, locked recording only) sends one unmodified
  Return to the same verified process after a successful paste. Never on
  failure, empty text, cancellation, changed foreground or intervening input;
  Paste Last never replays it. The event tap predicts Enter suppression with
  the listener's own gesture machine; out-of-band suspend, recovery and
  cancellation invalidate that prediction through a reset epoch.
- Shortcut Reset restores only the selected shortcut through the existing
  defaults and save-before-apply path, and rejects conflicts with the other
  one.
- Dictation starts only once Microphone, Input Monitoring, Accessibility and a
  usable model key are ready. Recording audio behaviour and sleep prevention
  begin only after the intentional-hold threshold.
- Restore output volume only while Endu still owns the level; never overwrite
  a manual volume or mute change.
- Keep the channel mix until the user selects a channel. Bind that choice to
  the device UID, apply device and channel changes only between clips, and
  fall back to the mix with a visible warning. Never infer a voice channel
  from volume.
- Diagnostics and the HUD meter never modify or retain audio. No normalization,
  AGC, high-pass, VoiceProcessing or denoising by default without a separate
  user decision.
- New defaults only fill missing fields; saved choices are never rewritten.

### Providers and credentials

- API keys never reach argv, logs, temporary files, exports, previews or
  tests. Keychain access goes through `/usr/bin/security -i` (trade-off in
  `providers::keys`); existence checks read attributes and never decrypt. One
  account per provider under the existing service; never delete and recreate
  existing items. An environment variable overrides the Keychain; only
  OpenRouter keeps its legacy plaintext `api_key`. Never send an OpenRouter
  key to a direct provider.
- Every transport validates the parsed URL host before resolving a key: HTTPS,
  except loopback HTTP; reject lookalike hosts, URL credentials and
  backslashes. Redirects never forward credentials.
- `provider::model` is a direct model; `vendor/model` always means OpenRouter.
  Never infer native access from the vendor segment. Capabilities are per model
  and transport; unsupported fields never enter a request. Never invent a
  language for Auto. Saved `microsoft::` IDs are dropped on load and never
  routed to OpenRouter.
- `openrouter.json` is re-read on every dictation; the chain is the primary
  and at most one fallback (`openrouter::form::MODEL_SLOTS`), and any primary
  error falls back. Only the primary opens a live session; the fallback
  receives the finished recording as a file, so live-only models are
  primary-only.
- A live transcript completes only on the provider's explicit terminal
  acknowledgement after Finish. Never promote an interim segment, a segment
  commit or a socket close to a result, and never paste partial text. Live PCM
  comes from the authoritative Recording prefix, never the HUD meter. Queue
  gaps, overshoot past the sealed clip end (release edge plus the deliberate
  tail), disconnects and uncertain writes invalidate the whole live transcript;
  the exact clip is kept for fallback. Per-provider protocol:
  `providers::streaming`.
- Upload format per model: `providers::batch::accepts_opus`. Never resend a
  request the server may have received whole (`batch::post`). Warm-ups never
  carry credentials, audio or vocabulary and never run in tests.
- Render paths never resolve keys or make requests; selectors use cached key
  availability. Missing keys hide choices but keep saved selections.
- Per-model option profiles initialize once and are never overwritten on
  reselection or removal; an explicit persisted `false` stays off.
- Vocabulary capability probes use only the bundled synthetic fixture and
  invented names, outside capture. Never probe with user recordings or names,
  and never log bodies, keys or raw probe errors. HTTP 200 or metadata alone
  never proves hint support (`openrouter::vocabulary_support`).

### Output, History and recovery

- Output keeps submission order. A new capture never cancels accepted work.
  Escape cancels the capture (including one in its release tail) or the newest
  unfinished job; cancelled output never pastes, copies, sends Return, enters
  History or stays for Paste Last.
- Each accepted dictation and Retry snapshots its post-processing and
  vocabulary; formatting runs once before output. Empty formatted text never
  pastes, copies, sends Return or enters History. History is never
  reformatted retroactively. Post-processing is local and deterministic, every
  rule defaults off, and it never adds a model call.
- Capture the destination at recording start and verify it is still the
  foreground application immediately before writing the clipboard; send the
  paste to that process only. Protection is per application. A mismatch
  defers insertion without touching the clipboard; the opt-in copy on failure
  never runs on cancellation or shutdown. Deferred output keeps only the
  latest result in memory; Paste Last captures a new destination and records
  History on its first successful paste only.
- Completed clips are persisted privately before the remote attempt. Failed
  clips stay until recovered or explicitly deleted, untouched by History
  retention or Clear. Persist recovered text before removing audio. Retry never
  auto-pastes, is single-flight, uses current Models settings and keeps the
  originating application. Persist safe error categories and statuses only,
  never raw payloads or credentials. A disk failure keeps the audio in memory
  with a visible warning, never a false durability claim.
- History records only successful pasted output, bounded and retained per its
  setting. It stores counts and outcomes, never keyword or prompt contents.

### Statistics and privacy

- Statistics hold daily totals only: never text, audio, prompts, keyword
  contents, raw errors or individual requests. Recording them never affects
  dictation. They measure the transcription, before formatting.
- Missing cost is unknown; explicit zero is known. Estimates come only from
  `providers::pricing`, stay in their own fields and are always marked ≈.
  Never give old records modes, percentiles, costs or estimates they did not
  contain. Field semantics: `openrouter::stats`.
- `preferences_transfer` is an allowlist: never export keys, endpoints,
  History or retention, audio, permissions or login registration. Imports
  validate everything before writing, preserve local credentials and
  endpoints, roll back a partial save and apply runtime only after both files
  succeed.

### HUD

- The HUD is observational and click-through. Preparing follows the audio
  owner's real readiness, never a timer; ignore late readiness from a
  cancelled or older capture. Explicit opt-outs survive reloads and transfers.
- Never activate Endu to show an overlay or resurrect an idle or cancelled one.
  Cover Space and drawable faults with fault-injection tests, not GPU timing
  sleeps.

## UI conventions

- Every pane renders the `desktop_ui` scaffold (`pane_header` or
  `pane_header_with_action`, one column bounded by `PANE_CONTENT_WIDTH`) and
  builds controls from its primitives and metrics. Anything custom is wrapped
  in `layout_item`/`layout_container` so the every-pane layout test checks it.
  Text beside a fixed control shrinks and wraps.
- Settings save on edit. No Apply/Save buttons and no Saved/success rows: the
  selected option or updated field confirms. Text fields save when editing
  finishes (blur or Enter, once) and Escape cancels; never persist partial
  text or treat imports as user edits. Errors and explicit key-test results
  stay visible beside their control.
- Menus support Tab, arrows, Enter and Escape and return focus to their
  trigger. Setup never lets focus reach hidden settings.
- Colors are `desktop_ui::ThemeColor` tokens through `desktop_ui::rgb`; never a
  color that works in only one appearance. Appearance choices default to
  System. Section titles are sentence case.
- Animations use `desktop_ui::animate_once`/`animate_loop` or a settling
  spring, all honouring Reduce Motion (always in tests).
- Copy: explain credential storage once above the provider list and keep
  source-specific warnings beside each key. Use one section note for shared
  instructions. Do not repeat a selected preset as a subtitle. Model-specific
  prices and restrictions go right below that model's selector.
- `i18n`: English text is the key (`t("…")`, `tf!`); a test fails on a
  missing translation. Never translate identifiers, serialized values, logs or
  text the code compares; translate at display.
- Brand assets live in `resources/brand/`. The sidebar shows only the
  wordmark, with no tagline; the icon lives in the Dock and menu bar.

## Mandatory local validation before every commit

All validation runs locally on macOS before committing. GitHub Actions is for
releases only. Before **any** development commit, run on the final tree:

```sh
scripts/check-local.sh
```

It must pass. It runs `cargo fmt --check`, `cargo clippy --all-targets -D
warnings`, the full `cargo test` suite, shell syntax checks, the Python
script tests and `git diff --check`. Use `--release` only when changing
release optimization or mode-dependent behaviour, or chasing a release-only
regression.

- Fix failures locally; never use GitHub to discover them. A release build
  is not evidence that tests pass. Record the command and result in hand-offs.
- Keep the toolchain, Cargo cache and `target/` stable; no routine
  `cargo clean`.
- The script isolates app data and strips provider key variables. Never point
  tests at the user's real settings, History, Statistics or credentials.
  Tests and previews never resolve keys or reach a real network: inject the
  transport, or use a server bound to 127.0.0.1 inside the test.
  Explicitly ignored native tests are manual checks; say so rather than
  claiming they ran.
- Requires macOS, Rust stable, Python 3.11+ and Xcode with the Metal compiler.
  Linux runs do not count. If unavailable, explain the blocker and leave the
  changes uncommitted.

## Build and preview

```sh
scripts/build-app.sh  # target/app/Endu-<version>.zip
cargo run -- preview <pane>  # settings, microphone, models, providers,
                             # post-processing, hud, history, statistics,
                             # onboarding, dictation-hud, paste-notice
```

Only macOS builds the app. Previews never touch real configuration,
credentials or the network; preview Retry uses fixtures.

## GitHub: releases only

- `.github/workflows/release.yml` may build, package, sign, publish a release
  and update `Casks/endu.rb`. Never add formatting, lint, test or other
  validation jobs, or call `scripts/check-local.sh`, from a workflow.
- The workflow runs only when `Cargo.toml` changes on `main` or on manual
  dispatch. An Ubuntu `plan` job decides; the macOS job runs only to publish
  an unpublished version, repair its cask, or on dispatch (use it to finish an
  interrupted release).
- The `Cargo.toml` version is the release version; keep `Cargo.lock` in sync.
  `4.0.0` becomes tag `v4.0.0`, title `Endu 4.0.0`, asset `Endu-4.0.0.zip`.
  Never append a run number or prefix, or bump the version automatically.
- The release bot's cask-only commit never changes code or runs checks.
- Published versions are immutable: never overwrite an asset, duplicate a
  release or downgrade the cask. Update the cask only after its asset is
  verified. Keep `main` installable.

## Naming and signing

- The app is **Endu**: `Endu.app`, executable `endu`, cask `endu`, repository
  `publi0/endu`. Keep bundle id `dev.publio.hex-openrouter`, the Keychain
  identifiers and the `hex-openrouter` data directory; they carry permissions,
  keys and History across the rename. The first Endu release removes
  `Casks/hex-openrouter.rb` and writes `cask_renames.json`; until then the
  legacy cask keeps `main` installable. Published Hex releases stay as they
  are.
- Release signing uses one persistent self-signed certificate, pinned in
  `app/release-signing.pem`. Build without signing secrets; then
  `scripts/code_signing.py` imports the identity into a disposable keychain,
  signs, removes it and packages. Passwords reach `security -i` through stdin
  only. The publisher verifies the downloaded asset's signature, including
  resumed drafts. Never fall back to ad hoc signing or generate a new identity;
  rotating it changes the app identity and can cost user permissions. Keep an
  encrypted backup of the full identity.
- The local packager rejects ad hoc or mismatched certificates; use
  `cargo run -- preview` for UI work and never install an unsigned
  `--prepare` intermediate over the user's app.
- This is not Developer ID or notarized. Never install a trusted root, change
  trust settings, reset TCC or grant macOS permissions; the user grants them.

## Diagnostics

- `~/Library/Application Support/hex-openrouter/logs/live.ndjson`: state,
  dictation phases, foreground context.
- `~/Library/Application Support/hex-openrouter/logs/process.log`: Rust,
  CoreAudio and provider diagnostics.
