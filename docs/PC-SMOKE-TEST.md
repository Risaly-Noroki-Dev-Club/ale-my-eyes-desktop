# PC release smoke test

Run this checklist on every supported operating system before publishing. Record the date, OS version, display layout/DPI, audio device, package hash, tester, and result for each row.

| Area | Procedure | Pass condition |
| --- | --- | --- |
| Package | Install or extract the platform package from a clean user account and launch the GUI. | The app starts without a package-local `config/config.json`; Settings shows the real user config path. |
| Text/UI | Submit a text question, pause/resume listening, show an answer, then approve and cancel separate harmless plans. | Every visible control triggers once and the UI remains responsive. |
| API key | Open Settings, reveal the key, wait 10 seconds, and close Settings. Ask a visual question immediately afterward. | The key is masked by default, hides automatically, and never appears in the captured preview or logs. |
| Microphone | Test weak speech, normal speech, and background noise on at least two input devices (44.1/48 kHz where available). Leave listening active for 10 minutes. | Speech start/end is consistent, WAV transcription succeeds, noise does not trigger repeatedly, and later commands still arrive. |
| Display mapping | At 100%, 150%, and 200% scaling, test center and four corners on a single display, a display to the right, and a display with a negative origin. Use mouse-move-only plans first. | The confirmation shows the final desktop coordinates; the pointer reaches the intended point and out-of-bounds plans are rejected. |
| Remote protocol | Pair a client, send text and a near-limit WAV, preview a harmless plan, confirm it, then try confirming the same request from a second client and after 2 minutes. | Valid commands complete; cross-session and expired confirmations fail; oversized/rate-limited requests return stable error codes before inference. |
| Settings hot update | Pair a client, change endpoint/model in desktop Settings, save, and send the next remote request without restarting. | The next request reaches the new mock endpoint/model. |
| Automation safety | Exercise a harmless click in a disposable test window and deny a second plan. | Only the approved plan executes and the audit log contains redacted lifecycle records. |

Automated prerequisites:

```bash
./scripts/verify-pc-issues.sh
cargo test --workspace --locked
./scripts/stress-pc-io.sh
./scripts/test-source-package.sh
```
