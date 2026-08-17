# PC Remediation Status

Updated: 2026-08-17

## Support and release status

The supported desktop targets are Windows and Linux/NixOS. macOS packaging, CI, release artifacts, and support claims have been removed. Android is delivered from the independent mobile repository.

The code-level P0 and P1 remediation is implemented and covered by automated tests. Release remains blocked on the native evidence listed below; automated coordinate and audio tests do not replace real displays, microphones, or NixOS evaluation.

| Original issue | Implementation and automated evidence | Native evidence still required | State |
| --- | --- | --- | --- |
| P0-1 local assistant UI unreachable | `CompactScreen` is the root content; visible-control callback smoke passes | Launch packaged Windows and Linux GUI and exercise text, listening, preview, and confirmation | Automated complete |
| P0-2 model coordinates unsafe | Screenshot metadata, coordinate mapping, bounds rejection, virtual-desktop `SendInput`, right/negative monitor tests | Windows 100/150/200% scaling with right-side and negative-origin monitors | Automated complete |
| P0-3 microphone format differs from VAD | Input is downmixed and resampled to 16 kHz mono; 44.1/48 kHz chain tests pass | Two real microphone devices, weak/normal speech, and background noise | Automated complete |
| P0-4 VAD cursor stalls after trimming | Absolute sequence cursor and retained-buffer logic pass repeated-capacity stress | Ten-minute continuous listening session | Automated complete |
| P1-1 remote server keeps stale engine | Settings mutate the shared engine in place; post-start endpoint/model/key integration test passes | Change settings while a real Android session is connected | Automated complete |
| P1-2 remote limits and isolation | Connection/request/rate/frame/text/audio/pending limits, TTL, pairing throttling, per-session plans, and bounded-client stress pass | LAN interruption and concurrent-client native run | Automated complete |
| P1-3 API key exposure | Password input, timed reveal, and capture suspension while sensitive UI is visible | Confirm packaged GUI behavior and screen-capture exclusion | Automated complete |
| P1-4 public HTTP endpoint | Public HTTP is rejected and loopback HTTP is allowed; configuration tests pass | None | Complete |
| P1-5 main-flow integration | Mock HTTP success/failure tests, Noise loopback, coordinate-to-confirmation chain, and cancellation regression pass | Full Android v3 to real desktop workflow | Automated complete |
| P2-1 experimental product claims | Production defaults and README distinguish the scheduler safety path from unavailable local VLM runtimes | Native GPU runtime acceptance remains pending | In progress |
| P2-2 packaging and release | Windows/Linux/source scripts, Windows verifier, Linux smoke, Nix Flake/module, and release workflow updated | Native Windows package, Linux Xvfb, and `nix flake check`/build/module evaluation | Pending native evidence |

## Remote protocol v3

- Desktop and Android use protocol version 3 and identical golden JSON fixtures.
- The wire protocol supports persistent Noise-over-WebSocket sessions, text commands, PCM16 `AudioStart`/`AudioChunk`/`AudioEnd`, cancellation, confirmation, errors, and ping/pong.
- Audio is mono PCM16 at 8-96 kHz, with 24,576-byte decoded chunks and a 60-second limit.
- The desktop validates sequence, byte limits, total frames, and SHA-256 before creating a WAV or invoking ASR.
- Inference runs in a cancellable per-connection task. The connection continues to process cancellation and heartbeat messages; dropping or disconnecting the request prevents a late preview.
- Older protocol versions have no fallback. A version mismatch returns `PROTOCOL_INCOMPATIBLE`.
- v3 adds progress updates, explicit yes/no decisions, and separately redacted display and speech output.
- Model scheduling implementation status is tracked in `docs/MODEL-SCHEDULER-STATUS.md`.

## Automated evidence

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo check --workspace --locked` | Passed |
| `cargo test --workspace --locked` | Passed: 85 core, 44 GUI, 9 modeld unit tests, and 1 real-process IPC test; one real-display test ignored |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Passed |
| `scripts/verify-pc-issues.sh` | Passed |
| `scripts/stress-pc-io.sh` | Passed |
| Offline source-package check | Passed in a clean extracted source tree with `--locked --offline` |
| Windows GNU workspace check | Passed for `x86_64-pc-windows-gnu` |
| Windows GNU test target link (`--no-run`) | Passed for all workspace test binaries |
| Windows release package verifier | Passed: PE GUI/CUI subsystem, resources, archive contents, and runtime DLL checks |
| Linux Xvfb smoke | Requires Linux host |
| Nix Flake/package/module checks | Requires Nix-enabled Linux host |

The rebuilt Windows archive SHA-256 is `688e492904b04916ff1578a4624c7136228f36190dd0ca1b896e591960496882`. This is cross-build evidence only; the package has not been launched on a native Windows host.

## Native acceptance record

Record the OS version, hardware, display topology/scaling, microphone format, package hash, and result for every native run. Until all rows below have evidence, the corresponding platform is not release-complete.

| Platform | Required scenario | Result |
| --- | --- | --- |
| Windows | Package launch; 100/150/200% scaling; right and negative monitors; 44.1/48 kHz microphones; ten-minute listening | Pending |
| Linux | Package launch under X11/Xvfb and a real desktop; CLI status; TCP 37654; microphone and automation smoke | Pending |
| NixOS | `nix flake check --all-systems`, x86_64 package build, aarch64 evaluation, module evaluation, GUI/CLI launch | Pending |
| Android interoperability | Scan desktop v3 QR, 1/10/60-second audio, preview, decisions, reject/confirm, TTS, cancel, timeout, and disconnect | Owned by the independent Android validation task |
