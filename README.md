# Ale, My Eyes! Desktop

Ale, My Eyes! helps users understand the computer screen and carry out tasks through voice. Pair it with the companion Android app to ask questions, request actions, and review an operation plan before confirming execution.

## Quick start

With Rust and the required platform dependencies installed, run these commands from the desktop repository:

```bash
cargo build -p ale-gui -p ale-modeld --locked
cargo run -p ale-gui
```

Configure your models in Settings, pair your phone (on the same LAN), and start a voice request. Cloud providers require an API key; local models run inside `ale-modeld` and require separate model downloads plus a Vulkan-capable llama.cpp runtime.

On Windows, run `scripts\build-windows.cmd` from the repository directory instead. It checks and offers to install MSVC, the Windows SDK, Rust and LLVM without requiring WinGet. After building it copies the native Sherpa runtime DLL beside the release executables, then asks whether to create `ale-my-eyes-windows.zip`. For an unattended build with prerequisites already installed, run `powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -SkipInstall -BuildOnly`; add `-Package` instead of `-BuildOnly` to create the ZIP. Installer files can be supplied with `-VsInstallerPath`, `-RustupInstallerPath` and `-LlvmInstallerPath`.

## Development

Built with **Rust** and **Slint**:

- `ale-core` — configuration, remote cloud model calls, task harness runtime, and the Misaka-Network v4 protocol types
- `ale-gui` — desktop interface, phone remote server, automation, and Windows UI Automation adapters
- `ale-cli` — command-line tools
- `ale-modeld` — local model scheduling, SenseVoice ASR, and the GPU-only llama.cpp worker

Local inference never links into `ale-core`; it runs in the `ale-modeld` process (and its disposable SenseVoice worker).

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo test -p ale-core
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/verify-pc-issues.sh
```

The device-independent task harness can be exercised without a desktop session or cloud credentials:

```bash
cargo run -p ale-cli --locked -- harness-demo --task zoom --scenario normal
cargo build -p ale-cli --locked
python3 scripts/evaluate-harness.py
```

The desktop request path can run verified tasks on Windows through UI Automation: the cloud model proposes semantic steps with verifiable postconditions, the user confirms on the phone, and the harness observes, authorizes, executes, and verifies each step. The phone preview carries `task_mode=verified` plus a human-readable verification list; the desktop Work page keeps its regular confirmation text and action steps. The feature is enabled by default and can be switched off in Settings (`verified_tasks.enabled`). Other platforms, unavailable UIA, or a model that does not return the verified-task tool transparently fall back to the existing plan-then-execute path and record a `verified_task_unavailable` diagnostic. Windows real-device acceptance is still pending. See [HARNESS.md](HARNESS.md) for the runtime contract, offline evaluation matrix, fallback reasons, and acceptance boundary.

The phone connection uses **Misaka-Network v4**: pairing QR scheme `misaka-network://`, mDNS service `_misaka-network._tcp`, and the `misaka-network-remote-v4` Noise PSK label. `ale-core/src/remote.rs` is the canonical message definition; see the mobile repository's `docs/REMOTE-PROTOCOL-V4.md` for the wire contract.

## Packaging

- Linux: `./scripts/package-linux.sh` (run on Linux) produces `ale-my-eyes-linux.tar.gz`.
- Windows: `./scripts/build-windows.cmd` or `./scripts/package-windows.sh` on Windows/MSVC produces `ale-my-eyes-windows.zip` plus a symbols archive.
- Source: `./scripts/create-release.sh` produces `release/ale-my-eyes-source.tar.gz` and quickstart archives.
- CI: [`.github/workflows/build.yml`](.github/workflows/build.yml) runs fmt/check/test/clippy, the offline harness matrix, packaging smoke tests, and release assembly.

macOS and Android packaging are not part of this repository; the Android client lives in the [companion repository](https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-mobile).

## Nix

The flake builds the desktop binaries with the ASR-free feature set and provides a NixOS module:

```bash
nix build .#packages.x86_64-linux.default
nix develop
```

```nix
{
  imports = [ (builtins.getFlake "github:Risaly-Noroki-Dev-Club/ale-my-eyes-desktop").nixosModules.default ];
  programs.ale-my-eyes.enable = true;
}
```

The Nix package omits `ale-modeld`'s optional SenseVoice feature because the sherpa-onnx runtime fetches native archives outside the sandbox; set `programs.ale-my-eyes.package` to a locally built package if local ASR is required.

The app is under active development and may misunderstand speech or screen content. Review proposed actions carefully.

[Android companion app](https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-mobile)
