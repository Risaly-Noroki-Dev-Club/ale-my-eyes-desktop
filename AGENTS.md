# AGENTS.md

## Repository Shape
- Rust workspace members: `ale-core`, `ale-cli`, `ale-gui`, and `ale-modeld` (see root `Cargo.toml`). Make source changes in these crates, not in generated `release/`, `dist/`, or package snapshots.
- `ale-core` owns configuration, remote cloud model calls (`cloud.rs`/`model_api.rs`), scheduling and IPC types, the Misaka-Network v4 protocol (`remote.rs`), and the task harness (`harness/`). It contains **no local inference**: no ASR/VLM/LLM/TTS/VAD modules, and `asr.rs`/`vlm.rs`/`llm.rs`/`tts.rs`/`vad.rs`/`memory.rs`/`inference.rs` were removed.
- `ale-gui` is the desktop binary and Slint UI (`ui/`). It also hosts the phone remote server, screen capture/automation, the Windows UI Automation adapter used by verified tasks, and the `ale-modeld` supervisor. This repository has no Android/iOS target; the mobile client is a separate checkout.
- `ale-cli` exposes `transcribe`, `describe`, `test-connection`, `status`, and `harness-demo` (`cargo run -p ale-cli -- --help`).
- `ale-modeld` runs local inference out of process: a GPU-only `llama-server` worker for Qwen2.5-VL/ShowUI plus an optional SenseVoice ASR feature (default `sensevoice`; excluded on `windows-gnu`). It is launched by `ale-gui` with a bootstrap token over a Unix socket or Windows named pipe.
- The root `Cargo.toml` patches `i-slint-backend-winit` to `vendor/i-slint-backend-winit-1.16.1`; that vendor patch adds the macOS CADisplayLink frame throttle. Do not remove or bypass it casually.
- Protocol source of truth: `ale-core/src/remote.rs` (Misaka-Network v4, `REMOTE_PROTOCOL_VERSION = 4`, QR scheme `misaka-network://`, mDNS `_misaka-network._tcp`, chunk magic `MSKN`, PSK label `misaka-network-remote-v4`). Golden fixture: `ale-core/tests/fixtures/remote-v4.json`. The mobile repository has an independent implementation and must be validated in lockstep.
- Ignore build/runtime artifacts: `target/`, `dist/`, `release/`, `Ale, My Eyes!.app/`, `*.dmg`, `*.apk` (not produced here), `ale-my-eyes-*` package dirs/archives, model files, and `config/config.json`.

## Verification Commands
- CI definitions are real: `.github/workflows/build.yml` (jobs: `check` incl. clippy, `harness-evaluation`, `build-windows`, `build-linux`, `nix`, `source-package`, `release`), with the toolchain pinned to Rust 1.96.0. It triggers on `main`/`master` pushes, PRs, tags and manual dispatch. The current `refactor/misaka-network-v4` branch is **not pushed**, so no CI run exists for it: cite the workflow as the intended gate, never as a green build result for this branch.
- Format: `cargo fmt --all -- --check`.
- Whole workspace: `cargo check --workspace --locked`; tests: `cargo test --workspace --locked`; lint: `cargo clippy --workspace --all-targets --locked -- -D warnings`.
- Focused: `cargo test -p ale-core`, `cargo test -p ale-core <test_name>`, `cargo check -p ale-gui`, `cargo check -p ale-modeld`.
- Task harness (device-independent, no desktop session or cloud key needed):
  ```sh
  cargo run -p ale-cli --locked -- harness-demo --task zoom --scenario normal
  cargo build -p ale-cli --locked
  python3 scripts/evaluate-harness.py   # 24 cases: 3 tasks x 8 scenarios
  ```
- Static regression checks: `bash scripts/verify-pc-issues.sh` (UI/platform/protocol-v4/harness/packaging assertions plus removed-local-inference guards).
- Linux workspace checks need the native packages installed by CI (see the apt line in `.github/workflows/build.yml`).

## Build Notes
- `ale-gui` uses Slint `1.16`; `ale-gui/build.rs` compiles `ale-gui/ui/app.slint`, which imports the other `.slint` files.
- Desktop capture/automation uses `xcap` and `enigo`; the phone supplies audio. The desktop no longer captures a microphone or plays local TTS.
- Windows is the only platform with the verified-task UIA adapter (`ale-gui/src/uia_environment.rs`, PowerShell bridge). Non-Windows builds compile but report UIA unavailable.
- Full Windows packages require MSVC (`msvc` target; PDBs and Sherpa DLLs are collected into the ZIP). `windows-gnu` builds are validation-only and have no local SenseVoice ASR.

## Task Harness and Verified Tasks
- Runtime: `ale-core/src/harness/`; simulation/CLI reference: `harness-demo`; production wiring: `ale-gui/src/{verified_task,verified_planner,verified_authorizer,uia_environment}.rs` and `remote_server.rs`.
- Verified mode is opt-in via `verified_tasks.enabled` (default `true`) and only activates when the request may disclose the screen to the cloud and the platform exposes UI Automation (Windows). Otherwise the legacy plan-then-execute path is used and `verified_task_unavailable` is recorded in diagnostics. Observation-driven replanning is a separate host opt-in: `verified_tasks.allow_observation_steps` (serde default `false`) maps to `Task.allow_observation_steps`; the fixed-step production flow and `harness-demo` keep it off.
- The model may only propose semantic steps with verifiable postconditions (`propose_verified_task`); the host builds the `Task`, owns success criteria/budgets, and the runner requires user confirmation, re-observation, authorization, and post-action verification. Terminal states include `completed`, `needs_user`, `outcome_unknown`, `cancelled`, `failed`.
- Windows UIA real-device acceptance is **not done**; offline tests use mock bridges. Do not claim production success rates, UIA reliability, or real-task quality from the 24-case offline matrix.

## Architecture Notes
- Long-lived app config is created under the user config directory at `ale-my-eyes/config.json`; API keys use the system credential store. When that backend is unavailable (headless/container, no D-Bus/Secret Service), loading and saving degrade to warnings (`ConfigManager::credential_warnings`) instead of failing, so read-only CLI commands still work; credentials then only live in memory. Model scheduler runtime config points at `models/.runtime` (llama-server + GGUF files and the calibrated `runtime-capabilities.json`).
- `ale-core/src/cloud.rs` has one `RemoteCloudApi` implementing four wire protocols. `provider` is only a UI preset (`openai`, `anthropic`, `google`, custom); `wire_api` is the transport truth, and preset/protocol conflicts are rejected by config validation. Custom providers must set an explicit `wire_api`. There is no `retry_count` field and no desktop TTS.
- `ale-modeld` protocol: protobuf-framed IPC (`ale-core/src/model_ipc.rs`, version 2, 32 MiB frame cap) over a Unix socket/named pipe. GUI spawns it, pins configuration snapshots per request, enforces health/restart backoff, and falls back to cloud paths when local capabilities are unavailable.
- Screen/automation: `ale-core/src/actions.rs` defines actions; desktop execution lives in `ale-gui/src/automation.rs` and `platform/`. Verified-task execution goes through the UIA adapter instead of the legacy automation path.

## Packaging
- `./scripts/package-linux.sh` (Linux only), `./scripts/package-windows.sh` (Windows/MSVC or explicit `WINDOWS_TARGET`; GNU needs `ALLOW_LIMITED_WINDOWS=1`), and `./scripts/build-windows.ps1`/`build-windows.cmd` build runnable packages. They delete/recreate repo-root package dirs; run only for packaging.
- `./scripts/create-release.sh` deletes/recreates `release/` (source tarball now includes `HARNESS.md` and `MODEL-CALLING.md`). Do not use it as a verification substitute.
- No macOS or Android packaging exists in this repository; `scripts/package-macos.sh` and `package-android.sh` are intentionally absent.

## Commit Discipline
- Do not run `git commit`, `git reset`, `git checkout`, or `git stash` unless the user explicitly asks. Preserve unrelated working-tree changes.
- Never commit API keys, credentials, model weights, generated packages, or logs/dumps. Review `git status` before handing work back.
