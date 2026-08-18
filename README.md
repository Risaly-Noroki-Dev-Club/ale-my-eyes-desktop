# Ale, My Eyes! Desktop

Desktop application for Ale, My Eyes!, providing continuous voice interaction, screen understanding, and guarded desktop automation on Windows and Linux/NixOS.

This repository is intentionally independent from the mobile application. It owns its GUI, desktop runtime, CLI, and a private copy of `ale-core`; it has no source or Cargo dependency on `ale-my-eyes-mobile`.

## Model scheduler

The desktop owns the `ale-modeld` child process and communicates with it only through a private Unix socket or Windows named pipe. IPC is authenticated with a one-time token passed through inherited stdin. Remote credentials are loaded from the OS secret store and sent only over that authenticated session.

SenseVoiceSmall ASR is supported through sherpa-onnx and resamples mono PCM16 input to 16 kHz. Qwen2.5-VL, ShowUI, and UI-TARS remain gated until their GPU runtimes and pinned model packages are installed; missing local capability is reported to Android and never silently replaced by cloud inference. A remote model may return only a semantic plan, never executable coordinates.

On Windows, start the pinned high-memory model downloader with:

```bat
scripts\download-models.bat
```

Running without arguments, including by double-clicking the script, asks only whether to use unattended mode. It otherwise downloads all pinned models to the repository `models` directory with 8 concurrent Hugging Face workers. Use `sensevoice`, `qwen`, `showui`, or `uitars` instead of `all` for scripted single-model downloads. `--models-dir D:\AleModels` selects another drive and `--workers 16` changes the concurrent Hugging Face file count. The normal mode retries every minute for up to 24 hours; `--retry-hours 48` changes that window. Unattended mode accepts the displayed licenses, issues no further prompts, and retries indefinitely until every model completes or the process is manually stopped. Interrupted SenseVoice archives and Hugging Face shards resume from their partial data, and a model is marked complete only after the pinned snapshot finishes. The complete set requires about 63 GB of downloads and at least 70 GB of free space. Downloaded VLM files remain inactive until the pinned Vulkan calibration publishes passing capability evidence; file presence alone never bypasses that gate.

For the Radeon PRO W6800 real-machine bring-up, run the stages below in order. The pinned Vulkan test converts the existing snapshots to Q4_K_M locally, checks strict coordinate boxes, and creates redacted reports under `target\model-runtime-reports`. The modeld stage uses native MSVC binaries and performs no mouse or keyboard input.

```bat
scripts\model-runtime\setup-windows-test.bat
scripts\model-runtime\run-windows-amd.bat
scripts\model-runtime\run-windows-modeld.bat
scripts\model-runtime\run-controlled-window-test.bat
```

The controlled-window command is dry-run by default. Only after its report passes may a tester run `powershell -ExecutionPolicy Bypass -File scripts\model-runtime\run-controlled-window-test.ps1 -Execute`; that mode requires typing `YES` and can click only the dedicated WinForms fixture. This does not replace the desktop product-chain test through Android protocol v3.

## Development

```bash
cargo run -p ale-gui
cargo run -p ale-cli -- status
cargo test -p ale-core
```

## Nix and NixOS

The Flake provides packages for `x86_64-linux` and `aarch64-linux`, GUI and CLI apps, a development shell, and `nixosModules.default`.

```bash
nix run .
nix run .#cli -- status
nix develop
```

The NixOS module only installs the selected package:

```nix
programs.ale-my-eyes.enable = true;
```

It does not create a service, enable autostart, or place API credentials in the Nix store. macOS packaging and support have been removed.

The desktop remote server implements protocol v3 for the Android arm64 client. Older peers are rejected without fallback. Protocol v3 adds progress, explicit privacy/risk decisions, and separately redacted display and speech output.

See the mobile client at https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-mobile.
