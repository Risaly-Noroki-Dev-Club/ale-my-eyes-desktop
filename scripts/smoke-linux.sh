#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "smoke-linux.sh must run on Linux" >&2
    exit 1
fi
for command in cargo nc timeout xvfb-run; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "missing required command: $command" >&2
        exit 1
    }
done

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
runtime_dir=$(mktemp -d)
gui_pid=""
cleanup() {
    if [[ -n "$gui_pid" ]] && kill -0 "$gui_pid" 2>/dev/null; then
        kill "$gui_pid" 2>/dev/null || true
        wait "$gui_pid" 2>/dev/null || true
    fi
    rm -rf "$runtime_dir"
}
trap cleanup EXIT

cd "$repo_root"
export XDG_CONFIG_HOME="$runtime_dir/config"
export XDG_CACHE_HOME="$runtime_dir/cache"
export XDG_DATA_HOME="$runtime_dir/data"
export RUST_LOG=info

cargo run --locked -p ale-cli -- status
timeout 60s xvfb-run -a cargo run --locked -p ale-gui >"$runtime_dir/gui.log" 2>&1 &
gui_pid=$!

for _ in $(seq 1 60); do
    if nc -z 127.0.0.1 37654; then
        kill "$gui_pid" 2>/dev/null || true
        wait "$gui_pid" 2>/dev/null || true
        gui_pid=""
        echo "Linux GUI smoke passed: TCP 37654 is listening"
        exit 0
    fi
    if ! kill -0 "$gui_pid" 2>/dev/null; then
        cat "$runtime_dir/gui.log" >&2
        echo "ale-gui exited before the remote server became ready" >&2
        exit 1
    fi
    sleep 1
done

cat "$runtime_dir/gui.log" >&2
echo "ale-gui did not listen on TCP 37654 within 60 seconds" >&2
exit 1
