# Ale, My Eyes! Desktop

Desktop application for Ale, My Eyes!, providing continuous voice interaction, screen understanding, and guarded desktop automation on Windows and Linux/NixOS.

This repository is intentionally independent from the mobile application. It owns its GUI, desktop runtime, CLI, and a private copy of `ale-core`; it has no source or Cargo dependency on `ale-my-eyes-mobile`.

## Model scheduler

The desktop owns the `ale-modeld` child process and communicates with it only through a private Unix socket or Windows named pipe. IPC is authenticated with a one-time token passed through inherited stdin. Remote credentials are loaded from the OS secret store and sent only over that authenticated session.

SenseVoiceSmall ASR is supported through sherpa-onnx and resamples mono PCM16 input to 16 kHz. Qwen2.5-VL, ShowUI, and UI-TARS remain gated until their GPU runtimes and pinned model packages are installed; missing local capability is reported to Android and never silently replaced by cloud inference. A remote model may return only a semantic plan, never executable coordinates.

See [model scheduler status](docs/MODEL-SCHEDULER-STATUS.md) for implemented boundaries and remaining native acceptance work.

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
