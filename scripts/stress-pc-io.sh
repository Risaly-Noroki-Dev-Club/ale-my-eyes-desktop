#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

cargo test -p ale-gui --lib audio::tests::absolute_cursor_survives_buffer_trimming -- --exact
cargo test -p ale-gui --lib remote_crypto::tests::roundtrips_multi_megabyte_encrypted_message_under_stress -- --exact
cargo test -p ale-gui --lib remote_server::tests::request_rate_stays_bounded_under_stress -- --exact
cargo test -p ale-gui --lib remote_server::tests::pairing_failure_client_table_is_bounded_under_stress -- --exact
cargo test -p ale-core --lib config::tests::config_file_io_stress_preserves_latest_value_and_redacts_secret -- --exact
