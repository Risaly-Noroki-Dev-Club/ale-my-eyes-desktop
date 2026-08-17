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

require 'compact := CompactScreen' ale-gui/ui/app.slint P0-1-root-ui
require 'visible_assistant_controls_reach_root_callbacks' ale-gui/src/lib.rs P0-1-ui-smoke
require 'ScreenCoordinateSpace' ale-gui/src/screen_capture.rs P0-2-coordinate-space
require 'maps_right_and_negative_origin_monitors' ale-gui/src/screen_capture.rs P0-2-multimonitor-test
require 'InputNormalizer::new' ale-gui/src/audio.rs P0-3-normalizer
require 'microphone_format_to_wav_and_vad_chain' ale-gui/src/audio.rs P0-3-chain-test
require 'start_sequence' ale-gui/src/audio.rs P0-4-absolute-cursor
require 'absolute_cursor_survives_buffer_trimming' ale-gui/src/audio.rs P0-4-buffer-stress
require 'remote_context_uses_new_endpoint_model_and_key_after_settings_save' ale-gui/src/remote_server.rs P1-1-engine-hot-update
require 'remote_server: Option<remote_server::RemoteServerHandle>' ale-gui/src/lib.rs P1-1-server-lifecycle
require 'MAX_SECURE_MESSAGE_BYTES' ale-gui/src/remote_crypto.rs P1-2-message-limit
require 'rejects_reassembled_message_over_limit_before_json_parse' ale-gui/src/remote_crypto.rs P1-2-message-limit-test
require 'pending_plans_expire_and_are_isolated_per_session' ale-gui/src/remote_server.rs P1-2-session-test
require 'pairing_failure_client_table_is_bounded_under_stress' ale-gui/src/remote_server.rs P1-2-pairing-table-stress
require 'InputType.password' ale-gui/ui/settings-popup.slint P1-3-password-input
require 'set_sensitive_ui_visible' ale-gui/src/platform/desktop.rs P1-3-capture-suspension
require 'validate_cloud_api_transport' ale-core/src/config.rs P1-4-https-validation
require 'test_validate_cloud_api_rejects_public_http' ale-core/src/config.rs P1-4-url-test
require 'mock_openai_timeout_is_reported' ale-core/src/cloud.rs P1-5-http-tests
require 'vlm_coordinates_to_confirmation_to_automation_chain' ale-gui/src/remote_server.rs P1-5-automation-chain
require 'spawned_modeld_authenticates_and_serves_health_over_unix_socket' ale-modeld/tests/ipc_process.rs modeld-real-process
require 'REMOTE_PROTOCOL_VERSION: u32 = 3' ale-core/src/remote.rs remote-v3-version
require 'AudioStart' ale-core/src/remote.rs remote-v3-audio-start
require 'DecisionRequest' ale-core/src/remote.rs remote-v3-decision
require 'AssistantOutput' ale-core/src/remote.rs remote-v3-output
require 'AudioAssembler' ale-gui/src/remote_server.rs remote-v3-desktop-assembler
require 'AUDIO_HASH_MISMATCH' ale-gui/src/remote_server.rs remote-v3-integrity
require 'protocol_v3_matches_golden_fixture' ale-core/src/remote.rs remote-v3-golden-fixture
require 'ale-modeld' README.md model-scheduler-product-scope
require 'mode: "adaptive"' ale-core/src/config.rs model-scheduler-default
require 'real_pc_handler_does_not_bypass_disabled_model_scheduler' ale-gui/src/remote_server.rs model-scheduler-no-bypass
require 'Cargo.lock' scripts/create-release.sh P2-2-lockfile
require 'vendor' scripts/create-release.sh P2-2-vendored-patch
require 'Icon=ale-my-eyes' scripts/package-linux.sh P2-2-linux-icon
require 'test-windows-package.ps1' .github/workflows/build.yml P2-2-windows-native-smoke
require 'nixosModules.default' flake.nix P2-2-nixos-module
require 'programs.ale-my-eyes.enable' README.md P2-2-nixos-docs
require 'smoke-linux.sh' .github/workflows/build.yml P2-2-linux-smoke

if [[ -e scripts/package-macos.sh ]] || rg -q 'build-macos|AleMyEyes.dmg|package-macos' .github/workflows README.md; then
    echo 'FAIL P2-2: macOS delivery support is still present' >&2
    exit 1
fi
printf 'PASS P2-2-no-macos-delivery\n'

if [[ -e .github/workflows/android.yml ]] || rg -q 'package-android|build-android' .github/workflows; then
    echo 'FAIL P2-2: desktop workflows still contain Android packaging' >&2
    exit 1
fi
printf 'PASS P2-2-desktop-workflows\n'

bash -n scripts/create-release.sh scripts/package-linux.sh scripts/package-windows.sh \
    scripts/verify-pc-issues.sh scripts/stress-pc-io.sh scripts/test-source-package.sh \
    scripts/verify-windows-package.sh scripts/smoke-linux.sh
printf 'PASS packaging-shell-syntax\n'
