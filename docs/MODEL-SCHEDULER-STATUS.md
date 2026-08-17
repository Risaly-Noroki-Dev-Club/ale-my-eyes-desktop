# Model Scheduler Status

Updated: 2026-08-17

## Implemented and enforced

- `ale-modeld` is the fourth workspace crate and is packaged beside the GUI and CLI.
- The desktop launches one child process and connects over a mode-0600 Unix socket or a Windows named pipe. No model TCP listener is opened.
- A random 32-byte bootstrap token is passed through inherited stdin, checked in constant time, erased after authentication, and accepted for one connection only.
- Protobuf IPC frames are capped at 32 MiB and responses are correlated by request ID.
- Concurrent IPC permits cancellation while a model call is running. Per-stage work is bounded by the job deadline and 30-second stage limit.
- `ModelJob` carries its request ID, capability, priority, deadline, risk ceiling, snapshot ID, and explicit privacy grants.
- SenseVoiceSmall runs through sherpa-onnx on CPU on upstream-supported targets. Mono PCM16 WAV at 8-96 kHz is decoded and linearly resampled to 16 kHz.
- `SmartModelManager` exposes a pinned-package installation path. Consent is bound to model ID, license, download size, and atomic-install disk requirement; every artifact is size- and SHA-256-verified before rename and reverified before use.
- NVIDIA VRAM is probed through `nvidia-smi`; Linux AMD VRAM is probed through DRM sysfs. VLM capability is not advertised without a supported installed runtime.
- Planning policy enforces the 0.90/0.97 conservative thresholds, five-step/single-app/postcondition limits, no local high-risk planning, stale-snapshot rejection, and dual-grounder agreement rules.
- Primary and pre-authorized backup remote endpoints run inside `ale-modeld`; failover is reported to the caller. Full screenshot payloads require a matching per-job privacy grant.
- The desktop supervises `ale-modeld`, restarts it after a process disconnect, counts one failure per process instance, and blocks automatic restarts after three consecutive failures. An explicit mobile approval to retry remote inference clears a blocked restart budget.
- Android protocol v3 provides progress, yes/no decisions, and redacted assistant speech. Disabling or losing `ale-modeld` does not reactivate the historical direct-cloud remote execution path.
- The desktop conversation UI no longer offers the legacy cloud model an executable-coordinate tool. It remains information-only until snapshot-bound grounding is available.

## Not yet production-complete

The following capabilities are deliberately reported unavailable rather than simulated:

- Qwen2.5-VL-3B/7B native GPU inference and schema-constrained local planning.
- SenseVoice on the current Windows GNU packaging target; sherpa-rs 0.6.8 does not publish a compatible binary, so that build reports local ASR unavailable instead of silently substituting it.
- SenseVoice in the Nix package; the upstream sherpa build downloads native archives during compilation, so the sandboxed Flake currently builds `ale-modeld` without that feature.
- ShowUI-2B and UI-TARS-1.5-7B native grounding, escalation, and model-to-model agreement.
- Windows UI Automation and Linux AT-SPI2 tree capture/redaction.
- Curated release manifests containing the real fixed revisions and hashes for every required model, plus the consent-driven download/removal UI. The installer and manifest validation layer are implemented, but placeholder hashes are not shipped.
- Per-GPU generation queues, live OOM recovery, and 120-second multi-model LRU eviction.
- Snapshot-bound single-step coordinate execution followed by a fresh capture and Qwen verification.
- Local SenseVoice recognition of spoken yes/no decisions; Android yes/no buttons are implemented and remain the required fallback.
- The complete medium-risk and risk-change reconfirmation chain tied to recomputed desktop risk and the current snapshot.

Until those rows and the native Windows/Linux/NixOS/Android acceptance matrix are complete, the scheduler is a secure preview and remote semantic-planning path, not a production autonomous executor.
