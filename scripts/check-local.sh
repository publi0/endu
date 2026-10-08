#!/usr/bin/env bash
# Required local validation before every commit. Never run this in GitHub Actions.
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
  echo "Checks belong on the developer's machine, not in GitHub Actions." >&2
  exit 1
fi
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "The required pre-commit checks must run on macOS to cover the app." >&2
  exit 1
fi

release_tests=false
for argument in "$@"; do
  case "$argument" in
    --release) release_tests=true ;;
    *) echo "Usage: $0 [--release]" >&2; exit 2 ;;
  esac
done

run_step() {
  local label="$1"
  shift
  local step_started
  step_started=$(date +%s)
  echo "$label"
  "$@"
  echo "$label completed in $(( $(date +%s) - step_started ))s."
}

test_support=$(mktemp -d "${TMPDIR:-/tmp}/hex-local-check.XXXXXX")
trap 'rm -rf -- "$test_support"' EXIT
export HEX_APPLICATION_SUPPORT_DIR="$test_support"
unset OPENROUTER_API_KEY OPENAI_API_KEY DEEPGRAM_API_KEY ELEVENLABS_API_KEY
unset XAI_API_KEY GEMINI_API_KEY GOOGLE_API_KEY
unset MODEL_API_KEY

run_step "Formatting" cargo fmt --all --check
run_step "Shell syntax" sh -n scripts/build-app.sh scripts/write-cask.sh
run_step "Release and signing automation tests" python3 -m unittest discover -s scripts -p 'test_*.py' -v
run_step "Whitespace" git diff --check
run_step "Metal compiler" xcrun metal --version
run_step "Clippy" cargo clippy --locked --all-targets -- -D warnings
run_step "Unit and integration tests" cargo test --locked --timings
if [[ "$release_tests" == "true" ]]; then
  run_step "Optimized tests" cargo test --locked --release --timings
fi
echo "Local checks passed."
