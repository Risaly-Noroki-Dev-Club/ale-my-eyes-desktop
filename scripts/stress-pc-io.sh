#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

cargo test -p ale-gui --lib remote_crypto::tests::roundtrips_fragmented_encrypted_message -- --exact
cargo test -p ale-gui --lib remote_crypto::tests::rejects_reassembled_message_over_limit_before_json_parse -- --exact
cargo test -p ale-gui --lib remote_server::tests::request_rate_stays_bounded_under_stress -- --exact
cargo test -p ale-gui --lib remote_server::tests::pairing_failure_client_table_is_bounded_under_stress -- --exact
cargo test -p ale-core --lib config::tests::config_file_io_stress_preserves_latest_value_and_redacts_secret -- --exact
