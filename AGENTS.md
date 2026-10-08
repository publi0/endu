# Hex Agent Guide

## Purpose

A slim macOS fork of HEX: tap a shortcut to lock recording or hold and release,
trim the silence, send
the clip to a speech provider with an ordered fallback chain, and paste the
transcript. Settings, Microphone, Providers, Models, Post-processing, HUD, History, and Statistics are the panes.
Keys and advanced request limits belong in Providers. Microsoft resource endpoints
and deployment are grouped inside the collapsed Advanced section. Explain credential
storage once above the provider list; keep source-specific warnings beside each key.
Use one section note for shared instructions and comparison periods. Do not repeat
selected preset values as subtitles; retain exact custom values and specific warnings.
Models contains the
primary/fallback chain, per-model language/streaming/options and shared keywords;
device/channel choices, priority, input levels, capture mode, silence trimming
and audio behavior while dictating belong in Microphone. Indicator position and
recording/transcription palettes belong in HUD. Everything
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
- `providers`: provider identities/capabilities, separate Keychain credentials, native
  batch adapters and bounded WebSocket workers. `providers_view` owns credentials
  and request limits; `model_options_view` lives in Models and exposes only
  compatible features.
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
- `context`: a retained foreground application identity captured when recording
  starts, plus the display name recorded in History.
- `paste_notice`: a short, click-through notice when automatic insertion is
  deferred because the foreground application changed.
- `history`: the owner-only bounded store of pasted text and metadata.
- `events`: bounded asynchronous NDJSON observations in `logs/live.ndjson`.
- `update_check`: detects when Homebrew replaces the installed bundle and offers
  a restart, using the app bundle enclosing the current executable.
- `app_settings`: persisted app settings and their live runtime projection.
- `app_window`, `desktop_ui`, `text_input`: the GPUI window and controls.
- `desktop`: the GPUI application, menus, menu bar item, HUD, and listener
  thread lifetime.
- `dictation_indicator`: the click-through Metal HUD.
- `hud_settings`, `hud_screen`, `hud_settings_view`: persisted HUD geometry,
  palettes, size, brightness, monitor selection and coherent runtime snapshots.
- `interaction_settings`, `sound_settings_view`: independent sound volumes and
  double-tap sensitivity, preserving legacy audible levels and timing defaults.
- `microphone_priority_view`: ordered Automatic input preferences; the audio
  owner applies device changes only between clips.
- `preferences_transfer`: a versioned, validated allowlist; never export keys,
  endpoints, History/retention, audio, permissions or login registration.
- `post_processing`, `post_processing_view`: local, deterministic formatting
  preferences and their settings UI. All rules default off; no extra model call.
- `status_item`, `login_item`, `onboarding`, `permission_guide`, `instance`,
  `feedback`: menu bar, launch at login, setup gate, permission helper, single
  instance lock, and tones.

## Invariants

- Capture never waits on transcription, paste, History, or statistics. Queues
  stay bounded. Starting a new capture never cancels accepted work, and output
  keeps submission order.
- Default TapOrHold locks on a clean release before 300 ms; the next press
  finishes. Holding at least 300 ms finishes on release. Hold and DoubleTap
  remain selectable legacy modes. DoubleTap uses the selected timing window
  (200/300/450 ms, default 300 ms); TapOrHold keeps its 300 ms hold threshold.
  A 450 ms pre-roll protects speech onset;
  captures shorter than 300 ms still discard. Escape cancels capture or the
  newest unfinished job, and cancelled jobs never paste.
- Optional Enter-to-submit is off by default and applies only to locked
  recording. Consume a bare Return/keypad Enter press and its repeat/release,
  then transcribe, paste, wait for paste consumption and post an unmodified
  Return to the same verified process. Never send it on failure, empty text,
  cancellation, changed foreground or intervening user input. Its interaction
  token belongs to one job; Paste Last must never replay it.
- The event tap predicts only Enter suppression using the same gesture
  machine as the listener, so a fast Enter cannot leak before a locking release
  is processed. Audio ownership remains in the listener. Out-of-band suspend,
  recovery and cancellation invalidate that prediction through a reset epoch.
- Reset restores only the selected shortcut, re-enables a disabled Paste Last,
  cancels any shortcut capture, and rejects conflicts with the other shortcut.
  Use the existing default bindings and save-before-apply path.
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
  Accessibility, and a usable configured model/provider key are ready.
- `Release microphone while idle` opens the device on the shortcut with no
  pre-roll and closes it once capture is idle.
- API keys never reach argv, logs, or temporary files. Keychain operations use
  Security.framework directly; native HTTPS/WebSocket transports hold credentials
  in memory. Preserve existing items; do not delete/recreate keys or claim that
  updating a value retroactively narrows a legacy item’s access list.
  Each provider has a separate Keychain account under the existing service.
  Every network transport validates the parsed URL host before resolving keys:
  HTTPS is required except for loopback HTTP. Prefixes such as localhost.evil,
  credentials in URLs and backslashes are rejected. Redirects must not forward
  credentials. Provider environment variables override its Keychain item; only OpenRouter
  retains its legacy plaintext `api_key` compatibility. Never send an OpenRouter
  key to a direct provider or export credentials. Preview/tests never resolve keys
  or use real network transports, even if another test sets the global config.
- `openrouter.json` is re-read on every dictation. Any model error falls back
  to the next model; a 429 asking to wait at most the configured time is
  retried once on the same model.
- Statistics record daily totals only, never text or audio, and recording
  them must never affect dictation. Per-attempt telemetry is ephemeral and folds
  into daily provider/model/mode counters and bounded latency histograms. Do not
  persist individual requests, prompts, keyword contents or raw errors.
  Keep one overview sample per transcription, including all chunks. Count retries
  on the same model separately from successful fallback to another chain slot and
  live-to-recorded recovery. Transcription success is not proof of a successful paste.
  Live means a session initiated during capture, not proof that its handshake or
  audio transmission completed before Finish. Recorded means a completed-clip
  attempt, including WebSockets started after recording. Compare their successful
  request latencies as Finish-to-final and request-to-response respectively. Mark P95 as
  approximate and require twenty measured successful responses. Never assign old
  records a request mode, percentile or cost coverage they did not contain.
  Missing reported cost is unknown; explicit zero is known. Do not invent price
  estimates. Group native models by their true provider; a vendor/model route is
  still OpenRouter. Filters for request comparisons must not silently change the
  scope of overview cards. Load one consistent period snapshot off the UI thread,
  preserve unreadable statistics and discard stale asynchronous reloads.
- Each accepted dictation snapshots its post-processing preferences. Format
  once before ordered output; History and Paste Last retain that result and
  its casing policy. Continuation must not undo explicit lowercase choices.
  Empty formatted text must never paste, copy, send Return or enter History.
  Retry snapshots current formatting preferences; if formatting removes all
  recovered text, preserve the source audio and show a safe explanation.
  Existing History entries are never reformatted retroactively. Statistics
  continue to measure the transcription, before local text formatting.
- Direct model IDs use `provider::model`; legacy `vendor/model` always means
  OpenRouter. Do not infer native access from the vendor segment. Capabilities
  are per model and transport, not per provider alone. Unsupported fields never
  enter a request. Keywords are shared across the whole configured chain; show
  them in Models if any primary/fallback supports vocabulary hints.
  Vocabulary names render as whole removable chips in both panes. Enter/blur
  commits a draft; multi-line/comma paste adds whole terms atomically; spaces
  remain inside a phrase. Empty-field Backspace removes one chip, Escape
  cancels the draft. Failed saves retain accepted chips and the pending draft.
  Other options live in persistent per-model profiles: initialize from compatible
  source options only once, never overwrite a profile on reselection/removal.
  Compatible streaming/formatting/cleanup toggles default on; explicit persisted
  false stays off. Newly supported target capabilities start with their defaults.
  Native Microsoft/Google model IDs remain distinct from identically named OpenRouter
  routes. Azure resource endpoints/deployment stay local across preference imports and
  never enter exports. Validate Azure HTTPS roots before resolving the Microsoft key.
  Microsoft streaming defaults to the Speech SDK-compatible custom resource route,
  sharing the batch endpoint/key. Only an explicitly populated legacy deployment
  selects Foundry Realtime. Speech frames correlate X-RequestId, retain only final
  phrases, and require Finish, EndOfDictation and turn.end before releasing text.
  A phrase final or socket close alone is never a completed recording. Preserve
  cancellation, bounded text retention and exact audio accounting on this route.
  Google uses the AI Studio key and dedicated Transcribe API with store:false for unary
  requests; do not persist Interactions or upload permanent files. Smart transcription
  is one atomic Google option. Grok formatting requires a supported explicit language;
  never invent one for Auto. Punctuation/numerals are separate capabilities.
  Only explicit final acknowledgements may complete a live transcript. Google needs
  transcription completion as well as turn completion; neither interim transcription
  nor generated assistant content is dictation text. Missing completion falls back.
  Grok can send definitive text in `transcript.partial` with `is_final=true`
  before an empty `transcript.done`. Retain finalized segments, reconcile whole
  utterances without duplication, and release the result only after Finish and
  the terminal acknowledgement. Never promote an interim segment or socket close
  to a completed transcript. Cover empty terminal text and absent confirmation
  through the provider dispatcher so valid streaming never triggers a second upload.
  Meta uses Muse Voice Transcribe's native PUSH_TO_TALK API: named languageBias,
  shared keywords and mono PCM16 at 16 kHz. Its WebSocket credential belongs only
  in the first JSON handshake, never HTTP headers or query parameters. Wait for
  its untyped sessionId acknowledgement before audio, pace PCM in real time,
  send endStream after the exact clip, and accept text only after final:true and
  server close 1000. Partials, speechEnd and abnormal/absent close are incomplete
  and must recover from the recorded clip. No prompt/formatting capabilities are
  documented. Unsupported explicit language hints are visible and rejected.
  Model search combines name, ID, provider and supported capabilities with AND
  between terms. Exact capability words must not match a model name that lacks
  that capability. Reuse cached keyword evidence; never probe on a search edit.
  Model-specific prices, prerequisites and restrictions belong immediately below
  that model's primary/fallback selector, not in a shared footer or every search
  result. Keep them conditional and specific to the selected transport/model.
  Selectors filter on cached provider-key availability, never resolving keys or
  making requests during render. Missing keys hide choices but retain saved
  selections/profiles with an unavailable notice. Nova-2 with Auto language uses
  batch until an explicit language is chosen; never invent a language.
- Live PCM comes from the authoritative, resampled Recording prefix, after the
  intentional threshold and before the conservative pending-input boundary.
  Never use the disposable HUD meter projection. Capture only uses bounded
  try_send; credentials, DNS, TLS, encoding and socket I/O stay off its thread.
  Queue gaps, overshoot from a delayed release, disconnects and uncertain writes
  invalidate the whole live transcript. The completed exact clip is retained
  for fallback and recovery. Cancel/discard/interruption/drop stop its session;
  after Finish, cancellation belongs to that pipeline job. Never paste partial
  text or end capture on a provider's segment completion. Final text still goes
  through ordered output, formatting, vocabulary and destination verification.
  Streaming is enabled by default when compatible, does not compact silence,
  and may send/bill silence before
  a final local silence check. Recovery WAV persistence happens after Finish.
  History stores actual provider/model/mode/keyword counts and attempt outcomes,
  never term/prompt contents; absent old metadata remains unknown.
  Preserve each request's provider-reported USD cost alongside its outcome,
  including charged failures/retries. Missing cost is not zero; never estimate
  historical prices. Mark sums as partial when coverage is incomplete, including
  bounded-away executions, and do not round positive small costs to zero.
- Vocabulary is snapshotted per accepted dictation and Retry. Apply existing
  formatting first, then restore canonical name spelling; carry its initial-case
  policy into Paste Last. Unknown/ambiguous local matches remain unchanged.
- Vocabulary capability checks run outside capture, with only the bundled
  synthetic fixture and invented names. Endpoint metadata and HTTP 200 alone
  cannot establish nested hint support. Require a matching negative control or
  reproducible A/B/A spelling effect, scoped to one discovered route and version.
  Never probe user recordings/names or log request bodies, keys, or raw probe errors.
  Multiple routes cannot be pinned on this endpoint and remain unverified.
  Cache schema/evidence with expiry; an inconclusive check means local-only.
  A runtime hint rejection retries without hints once within the original deadline.
  Unit tests inject transport and never run paid or authenticated requests.
- Normal History records only successful pasted output with seven-day default
  retention and hard caps. The History pane also exposes separate recovery
  entries, whose audio/text must not be pruned by those normal-history rules.
- Capture the destination at recording start. Immediately before writing the
  clipboard, verify it is still the foreground application. Send the shortcut
  to that verified process, never globally. A different or missing target
  defers insertion without touching the clipboard or continuation state by
  default. The explicit copy-on-paste-failure option may copy the raw transcript
  on detected failures/deferred output, never on cancellation/shutdown. This
  invalidates pending clipboard restores and does not record History or count
  as a paste. Silent recipient failures cannot be detected by CGEventPostToPid.
- Deferred output releases the ordered output worker and retains only the
  latest result in memory for explicit Paste Last. That action captures a new
  destination. Record History only on its first successful paste; repeated
  pastes must not duplicate entries. Cancellation/shutdown must not retain or
  paste cancelled output. Do not treat the foreground monitor as authorization.
- Destination protection is per application, not per window or text field.
- Completed clips are privately persisted before the remote transcription
  attempt for user-requested recovery. Failed/interrupted attempts remain in
  recording-recovery until recovered or explicitly deleted; never expire them
  via normal History retention or Clear dictations. Persist recovered text
  before removing audio. Manual Retry must not auto-paste, must be single-flight,
  and must use current Models settings. Keep the originating application from
  the capture context through Retry; never replace it with the History window's
  app. Persist safe error categories, HTTP status and timeout values, never raw
  provider/transport payloads or credentials. A disk-write failure retains session
  audio in memory with a visible warning, never a false durability claim.
- `recording_recovery` owns WAV/metadata persistence and the isolated manual
  retry worker. Preview retries must use fixtures, never network/credentials.
- The HUD is observational and click-through. Its preparing capsule follows
  the audio owner's actual readiness, never a timer. A warm microphone starts
  directly in Recording; a cold open emits a generation-scoped CaptureReady.
  Ignore late readiness after cancellation or a newer capture. Preserve the
  red recording and blue transcription defaults, animation, and tone threshold;
  transitioning from preparation must not restart the entrance animation.
  Voice reaction defaults on and only uses fresh meter samples during Recording.
  It never changes captured audio; an explicit opt-out stays off across reloads
  and preference transfers. Stale or invalid meter values must settle to rest.
  HUD choices apply only after successful saves, with old settings defaulting
  to Top/Red/Blue. Position uses visibleFrame to avoid the Dock; Paste Last's
  notice follows the same screen and edge. Palette changes affect the entire
  phase, including its glow and highlights, and never tint Preparing. Missing
  fixed displays or active-window metadata fall back without permission prompts.
  HUD and Paste Last notices observe active-Space changes on NSWorkspace's
  notification center. Re-register a visible overlay on the active Space without
  activating Hex or resetting its animation; do not trust a cached ordered flag
  as proof of native visibility. Never resurrect an idle/cancelled overlay on a
  Space change, and rate-limit repairs while WindowServer settles a transition.
  Rendering must remain live for recording/pending work when Metal cannot supply
  a drawable, but finished/cancelled HUDs must still settle and stop. Viewport
  maintenance must not wait for a drawable while holding up main-thread Space
  recovery. Cover these cases with fault-injection tests, not GPU timing sleeps.
- Imports validate all fields before writing, preserve local credentials and
  endpoints, serialize Models readers/writers, and roll back a partial save.
  Apply runtime only after both files succeed. Imports wait for key operations
  to finish so their callbacks cannot overwrite the imported editor state.
- Every pane renders the `desktop_ui` scaffold: `pane_header` or
  `pane_header_with_action`, then one column bounded by `PANE_CONTENT_WIDTH`.

- Choice menus support Tab, arrows, Enter, and Escape, restore focus to their
  trigger, and keep errors visible outside scrollable choices. Setup must not
  allow keyboard focus into hidden settings. Keep errors beside their control
  and disable incompatible key operations while one is pending.
  Across all panes, the selected option or updated field confirms a save: do
  not add Saved/success rows. Keep errors and explicit key-test results visible.
  Settings have no Apply/Save buttons. Text fields validate and save when
  editing finishes (focus leaves or Enter); Escape cancels the draft. Do not
  persist incomplete text while typing, duplicate a save on Enter then blur,
  or treat programmatic imports as fresh user edits.
  Reuse desktop_ui control metrics: 32-point height, 12-point control text,
  300-point pickers/segmented controls, and 96-point numeric inputs. Segments
  divide the available width evenly. Do not invent per-pane control sizes.
  Display selection is one menu with pointer/window modes and connected
  displays; refresh that list when opening it rather than adding a button.

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
- `python3 -m unittest discover -s scripts -p 'test_*.py' -v`
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
all provider API-key environment variables for the test process. Do not point tests at
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
cargo run -- preview microphone
cargo run -- preview models
cargo run -- preview providers
cargo run -- preview post-processing
cargo run -- preview hud
cargo run -- preview history --open-history-retention
cargo run -- preview statistics
cargo run -- preview onboarding
cargo run -- preview dictation-hud
cargo run -- preview dictation-hud --hud-position bottom --recording-color green --transcription-color purple
cargo run -- preview paste-notice
```

Only macOS builds the app. Other platforms can run the portable modules'
tests, but there is no Linux app. Previews must not access real configuration,
credentials, or network services. The History preview includes a synthetic WAV
in failed state; its Retry uses a fixed local response and updates through
normal History polling.

## GitHub: releases only

`.github/workflows/release.yml` may build, package, sign, publish a release,
and update `Casks/hex-openrouter.rb`. **Do not add formatting, linting, unit
tests, integration tests, or other validation jobs to GitHub Actions.** Do
not call `scripts/check-local.sh` or any test suite from a workflow. All of
those steps belong to the local pre-commit process above.

The workflow starts only when `Cargo.toml` changes on `main` or by manual
dispatch, never for code-only pushes. An Ubuntu `plan` job decides first; the
macOS job runs only to publish an unpublished version, repair its cask, or on
manual dispatch. Use dispatch to finish an interrupted release.

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

Release signing must use the same persistent certificate, pinned publicly in
`app/release-signing.pem`. Compile and prepare the bundle in a separate step
without signing secrets. Then `scripts/code_signing.py` imports the encrypted
identity from Actions secrets into a disposable keychain, signs, removes the
keychain, and packages the app. Passwords reach `security -i` through stdin,
never argv. Normal failures also clean up; forced termination relies on the
disposable GitHub-hosted runner. The publisher verifies the downloaded asset's
signature too, including resumed drafts. Never fall back to ad hoc signing in
Actions or regenerate an identity for each build. Rotating the certificate
changes the app identity and can require new user permissions.
The regular local packager also rejects ad hoc or mismatched certificates;
use isolated `cargo run -- preview` sessions for UI development. Never install
an unsigned `--prepare` intermediate over the user's signed app. Preserve an
encrypted backup of the full identity; the public PEM cannot restore the key.

The current distribution uses a self-signed identity, not Developer ID or
Apple notarization. Do not install a trusted root, alter trust settings, reset
TCC, or grant macOS permissions to make it work. The user grants permissions.
The local signing regression test uses disposable certificates and tiny test
apps to check continuity across builds and rejection of a different signer.

## Diagnostics

- `~/Library/Application Support/hex-openrouter/logs/live.ndjson`: state,
  dictation phases, and foreground context.
- `~/Library/Application Support/hex-openrouter/logs/process.log`: Rust,
  CoreAudio, and OpenRouter diagnostics.
