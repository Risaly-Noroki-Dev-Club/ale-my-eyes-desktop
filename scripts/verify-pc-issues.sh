#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

require() {
    local pattern=$1
    local path=$2
    local label=$3
    if ! rg -q "$pattern" "$path"; then
        printf 'FAIL %s: %s not found in %s\n' "$label" "$pattern" "$path" >&2
        exit 1
    fi
    printf 'PASS %s\n' "$label"
}

absent() {
    local path=$1
    local label=$2
    if [[ -e "$path" ]]; then
        printf 'FAIL %s: removed path still exists: %s\n' "$label" "$path" >&2
        exit 1
    fi
    printf 'PASS %s\n' "$label"
}

# Desktop UI and platform behavior.
require 'export component AppWindow inherits Window' ale-gui/ui/app.slint P0-1-root-ui
require 'visible_assistant_controls_reach_root_callbacks' ale-gui/src/desktop_ui.rs P0-1-ui-smoke
require 'ScreenCoordinateSpace' ale-gui/src/screen_capture.rs P0-2-coordinate-space
require 'maps_right_and_negative_origin_monitors' ale-gui/src/screen_capture.rs P0-2-multimonitor-test
require 'InputType.password' ale-gui/ui/settings-page.slint P1-3-password-input
require 'set_sensitive_ui_visible' ale-gui/src/platform/desktop.rs P1-3-capture-suspension
require 'remote_server::start' ale-gui/src/desktop_ui.rs P1-1-server-lifecycle
require 'remote_context_uses_new_endpoint_model_and_key_after_settings_save' ale-gui/src/remote_server.rs P1-1-engine-hot-update
require 'MAX_SECURE_MESSAGE_BYTES' ale-gui/src/remote_crypto.rs P1-2-message-limit
require 'rejects_reassembled_message_over_limit_before_json_parse' ale-gui/src/remote_crypto.rs P1-2-message-limit-test
require 'pending_plans_expire_and_are_isolated_per_session' ale-gui/src/remote_server.rs P1-2-session-test
require 'pairing_failure_client_table_is_bounded_under_stress' ale-gui/src/remote_server.rs P1-2-pairing-table-stress
require 'vlm_coordinates_to_confirmation_to_automation_chain' ale-gui/src/remote_server.rs P1-5-automation-chain
require 'real_pc_handler_does_not_bypass_disabled_model_scheduler' ale-gui/src/remote_server.rs model-scheduler-no-bypass
require 'mode: "adaptive"' ale-core/src/config.rs model-scheduler-default
require 'spawned_modeld_authenticates_and_serves_health_over_unix_socket' ale-modeld/tests/ipc_process.rs modeld-real-process

# Cloud provider presets and transport validation.
require 'fn effective_wire_api' ale-core/src/config.rs provider-preset-wire-api
require 'provider_presets_map_to_effective_wire_api' ale-core/src/config.rs provider-preset-mapping-test
require 'test_validate_cloud_api_rejects_public_http' ale-core/src/config.rs P1-4-url-test
require 'mock_openai_timeout_is_reported' ale-core/src/cloud.rs P1-5-http-tests

# Misaka-Network v4 protocol.
require 'REMOTE_PROTOCOL_VERSION: u32 = 4' ale-core/src/remote.rs misaka-v4-version
require 'misaka_v4_matches_golden_fixture' ale-core/src/remote.rs misaka-v4-golden-fixture
test -f ale-core/tests/fixtures/remote-v4.json
printf 'PASS misaka-v4-fixture-file\n'
require 'misaka-network://pair' ale-core/src/remote.rs misaka-v4-scheme
require '_misaka-network\._tcp\.local\.' ale-gui/src/remote_server.rs misaka-v4-mdns
require 'misaka-network-remote-v4' ale-gui/src/remote_crypto.rs misaka-v4-psk
require 'MSKN' ale-gui/src/remote_crypto.rs misaka-v4-chunk-magic
require 'OutcomeUnknown' ale-core/src/remote.rs misaka-v4-outcome-unknown
require 'NeedsUser' ale-core/src/remote.rs misaka-v4-needs-user
require 'pub task_mode: Option<String>' ale-core/src/remote.rs misaka-v4-task-mode
require 'pub verification: Option<Vec<String>>' ale-core/src/remote.rs misaka-v4-verification
require 'VerifiedStep' ale-core/src/remote.rs misaka-v4-verified-stage

# Task harness runtime, CLI integration, and verified-task production path.
require 'pub mod harness;' ale-core/src/lib.rs harness-runtime
require 'HarnessDemo' ale-cli/src/main.rs harness-cli
test -f scripts/evaluate-harness.py
printf 'PASS harness-evaluation-script\n'
require 'evaluate-harness\.py' .github/workflows/build.yml harness-ci
require 'propose_verified_task' ale-gui/src/verified_task.rs verified-task-tool
require 'ScriptedPlanner' ale-gui/src/verified_planner.rs verified-task-planner
require 'ConfirmedTaskAuthorizer' ale-gui/src/verified_authorizer.rs verified-task-authorizer
require 'UiaEnvironment' ale-gui/src/uia_environment.rs verified-task-uia-environment
require 'fn uia_available' ale-gui/src/uia_environment.rs verified-task-uia-availability
require 'harness::run' ale-gui/src/remote_server.rs verified-task-runner
require 'verified_tasks' ale-core/src/config.rs verified-task-config
require 'verified-tasks' ale-gui/ui/settings-page.slint verified-task-settings-toggle
require 'TASK_MODE_VERIFIED' ale-gui/src/verified_task.rs verified-task-mode
require 'VERIFIED_TERMINAL_DEADLINE' ale-gui/src/remote_server.rs verified-task-terminal-deadline
require 'verified_task_unavailable' ale-gui/src/remote_server.rs verified-task-fallback

# Packaging and distribution scope.
require 'Cargo.lock' scripts/create-release.sh P2-2-lockfile
require 'HARNESS.md' scripts/create-release.sh P2-2-release-harness-doc
require 'vendor' scripts/create-release.sh P2-2-vendored-patch
require 'Icon=ale-my-eyes' scripts/package-linux.sh P2-2-linux-icon
require 'test-windows-package\.ps1' .github/workflows/build.yml P2-2-windows-native-smoke
require 'smoke-linux\.sh' .github/workflows/build.yml P2-2-linux-smoke
require 'nixosModules.default' flake.nix P2-2-nixos-module
require 'programs.ale-my-eyes.enable' README.md P2-2-nixos-docs

# Removed local-inference stack must not return.
absent ale-core/src/asr.rs P3-1-no-local-asr
absent ale-core/src/vlm.rs P3-1-no-local-vlm
absent ale-core/src/llm.rs P3-1-no-local-llm
absent ale-core/src/tts.rs P3-1-no-local-tts
absent ale-core/src/vad.rs P3-1-no-local-vad
absent ale-core/src/blocking_worker.rs P3-1-no-local-worker
absent ale-core/src/manager.rs P3-1-no-model-manager
absent ale-core/src/downloader.rs P3-1-no-legacy-downloader
absent ale-core/src/memory.rs P3-1-no-memory-subsystem
absent ale-gui/src/audio.rs P3-1-no-desktop-audio
absent ale-gui/src/tts_player.rs P3-1-no-desktop-tts
absent ale-gui/src/file_picker.rs P3-1-no-file-picker
if rg -q 'local-inference|whisper-rs|^ort =|^tts =' ale-core/Cargo.toml; then
    echo 'FAIL P3-1: ale-core manifest still enables the removed local-inference stack' >&2
    exit 1
fi
printf 'PASS P3-1-no-local-inference-deps\n'
if rg -q 'mod (asr|vad|tts|vlm|llm|memory|manager|downloader|blocking_worker)' ale-core/src/lib.rs; then
    echo 'FAIL P3-1: ale-core/lib.rs still declares a removed module' >&2
    exit 1
fi
printf 'PASS P3-1-no-local-inference-modules\n'

if [[ -e scripts/package-macos.sh ]] || rg -q 'build-macos|AleMyEyes.dmg|package-macos' .github/workflows README.md; then
    echo 'FAIL P2-2: macOS delivery support is still present' >&2
    exit 1
fi
printf 'PASS P2-2-no-macos-delivery\n'

if [[ -e scripts/package-android.sh || -e scripts/build-android-java.sh ]] ||
    [[ -e .github/workflows/android.yml ]] ||
    rg -q 'package-android|build-android' .github/workflows; then
    echo 'FAIL P2-2: desktop repository still carries Android packaging' >&2
    exit 1
fi
printf 'PASS P2-2-desktop-workflows\n'

bash -n scripts/create-release.sh scripts/package-linux.sh scripts/package-windows.sh \
    scripts/verify-pc-issues.sh scripts/stress-pc-io.sh scripts/test-source-package.sh \
    scripts/verify-windows-package.sh scripts/smoke-linux.sh
printf 'PASS packaging-shell-syntax\n'
