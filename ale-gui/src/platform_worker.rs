//! Isolated desktop execution. Private pipe payloads never enter diagnostic logs.
#[cfg(any(windows, test))]
use crate::platform::ExecutionControl;
#[cfg(windows)]
use ale_core::actions::ActionPlan;
#[cfg(windows)]
use ale_core::{AleError, Result};
#[cfg(any(windows, test))]
use std::io::Read;
#[cfg(windows)]
use std::io::Write;
#[cfg(any(windows, test))]
use std::process::{Command, Output, Stdio};
#[cfg(any(windows, test))]
use std::time::{Duration, Instant};

#[cfg(any(windows, test))]
const OUTPUT_LIMIT: usize = 1024 * 1024;

#[cfg(any(windows, test))]
pub(crate) fn bounded_output(
    command: &mut Command,
    control: &ExecutionControl,
) -> std::io::Result<Output> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = ale_core::child_process::ManagedChild::spawn(command)?;
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("missing stdout"))?;
    let stderr = child
        .child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("missing stderr"))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(2);
    for (kind, mut pipe) in [
        (0, Box::new(stdout) as Box<dyn Read + Send>),
        (1, Box::new(stderr)),
    ] {
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut buffer = [0; 8192];
            let mut overflow = false;
            while let Ok(n) = pipe.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                if bytes.len() + n <= OUTPUT_LIMIT {
                    bytes.extend_from_slice(&buffer[..n]);
                } else {
                    overflow = true;
                }
            }
            let _ = sender.send((kind, bytes, overflow));
        });
    }
    drop(sender);
    let status = loop {
        if control.check().is_err() {
            child.kill_tree_and_wait(Duration::from_secs(2))?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "HELPER_CANCELLED_OR_TIMED_OUT",
            ));
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Close all owned descendants before awaiting their inherited pipe handles.
    child.terminate();
    let mut streams = [Vec::new(), Vec::new()];
    for _ in 0..2 {
        let (kind, bytes, overflow) = receiver
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| std::io::Error::other("HELPER_PIPE_STALLED"))?;
        if overflow {
            return Err(std::io::Error::other("HELPER_OUTPUT_LIMIT"));
        }
        streams[kind] = bytes;
    }
    let [stdout, stderr] = streams;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(windows)]
#[derive(serde::Serialize, serde::Deserialize)]
struct Request {
    plan: ActionPlan,
    approved: bool,
    remaining_ms: u64,
}
#[cfg(windows)]
#[derive(serde::Serialize, serde::Deserialize)]
struct Reply {
    actions: usize,
    error: Option<String>,
}

pub fn dispatch() -> Option<std::result::Result<(), String>> {
    if std::env::args().nth(1).as_deref() != Some("--desktop-execution-worker") {
        return None;
    }
    #[cfg(windows)]
    {
        Some(worker())
    }
    #[cfg(not(windows))]
    {
        Some(Err("Desktop worker is Windows only".into()))
    }
}

#[cfg(windows)]
fn worker() -> std::result::Result<(), String> {
    use std::io::BufRead;
    let name = std::env::var("ALE_EXECUTION_KEYS").map_err(|_| "WORKER_KEY_JOURNAL")?;
    keys::install(&name).map_err(|_| "WORKER_KEY_JOURNAL")?;
    let _ = ale_core::diagnostics::install_from_env("automation-worker");
    let input = std::io::stdin();
    let mut reader = input.lock();
    let mut line = String::new();
    (&mut reader)
        .take(OUTPUT_LIMIT as u64 + 1)
        .read_line(&mut line)
        .map_err(|_| "WORKER_INPUT")?;
    if line.len() > OUTPUT_LIMIT {
        return Err("WORKER_INPUT_LIMIT".into());
    }
    let request: Request = serde_json::from_str(&line).map_err(|_| "WORKER_INPUT_INVALID")?;
    drop(reader);
    let control = ExecutionControl::new(
        Instant::now() + Duration::from_millis(request.remaining_ms.min(120_000)),
    );
    let cancelled = control.clone();
    std::thread::spawn(move || {
        // EOF also cancels when the parent closes unexpectedly.
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        cancelled.cancel();
    });
    let result =
        crate::automation::AutomationEngine::new(Default::default()).and_then(|mut engine| {
            engine.execute_plan_controlled(&request.plan, request.approved, &control)
        });
    let reply = match result {
        Ok(result) => Reply {
            actions: result.actions_executed,
            error: None,
        },
        Err(error) => Reply {
            actions: 0,
            error: Some(error.to_string()),
        },
    };
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &reply).map_err(|_| "WORKER_OUTPUT")?;
    output.flush().map_err(|_| "WORKER_OUTPUT")?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn execute(
    plan: &ActionPlan,
    approved: bool,
    control: &ExecutionControl,
) -> Result<usize> {
    control.check()?;
    let payload = serde_json::to_vec(&Request {
        plan: plan.clone(),
        approved,
        remaining_ms: control.remaining().as_millis() as u64,
    })?;
    if payload.len() > OUTPUT_LIMIT {
        return Err(AleError::ConfigError("AUTOMATION_INPUT_LIMIT".into()));
    }
    let key_journal = keys::Journal::create()?;
    let mut command = Command::new(std::env::current_exe()?);
    command.env("ALE_EXECUTION_KEYS", &key_journal.name);
    if let Some(sink) = ale_core::diagnostics::current() {
        command
            .env("ALE_DIAGNOSTIC_DIRECTORY", &sink.directory)
            .env("ALE_DIAGNOSTIC_SESSION", &sink.session);
    }
    command
        .arg("--desktop-execution-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ale_core::child_process::ManagedChild::spawn(&mut command)?;
    let mut stdin = child
        .child
        .stdin
        .take()
        .ok_or_else(|| AleError::Other(anyhow::anyhow!("WORKER_STDIN")))?;
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| AleError::Other(anyhow::anyhow!("WORKER_STDOUT")))?;
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let success = stdin
            .write_all(&payload)
            .and_then(|_| stdin.write_all(b"\n"))
            .is_ok();
        let _ = sent.send((stdin, success));
    });
    let (output_send, output_recv) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(OUTPUT_LIMIT as u64 + 1).read_to_end(&mut bytes);
        let _ = output_send.send(result.map(|_| bytes));
    });
    let mut input = None;
    let mut cancelling = None;
    loop {
        if input.is_none() {
            if let Ok((pipe, success)) = received.try_recv() {
                input = Some(pipe);
                if !success {
                    control.cancel();
                }
            }
        }
        if control.check().is_err() && cancelling.is_none() {
            if let Some(pipe) = input.as_mut() {
                let _ = pipe.write_all(b"CANCEL\n");
            }
            cancelling = Some(Instant::now());
        }
        if cancelling.is_some_and(|start| start.elapsed() >= Duration::from_secs(1)) {
            child
                .kill_tree_and_wait(Duration::from_secs(2))
                .map_err(|_| AleError::Other(anyhow::anyhow!("EXECUTOR_REAP_FAILED")))?;
            key_journal.cleanup();
            return Err(AleError::Other(anyhow::anyhow!(
                "EXECUTION_CANCELLED_RESULT_UNCERTAIN"
            )));
        }
        if let Some(status) = child.try_wait()? {
            child.terminate();
            key_journal.cleanup();
            control.check()?;
            if !status.success() {
                return Err(AleError::Other(anyhow::anyhow!("EXECUTION_WORKER_FAILED")));
            }
            let bytes = output_recv
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| AleError::Other(anyhow::anyhow!("EXECUTION_PIPE_STALLED")))??;
            if bytes.len() > OUTPUT_LIMIT {
                return Err(AleError::Other(anyhow::anyhow!("EXECUTION_OUTPUT_LIMIT")));
            }
            let reply: Reply = serde_json::from_slice(&bytes)?;
            return match reply.error {
                Some(error) => Err(AleError::Other(anyhow::anyhow!(error))),
                None => Ok(reply.actions),
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// The parent can release only modifiers this worker may have pressed, even
// when a native call is killed before Rust stack unwinding runs.
#[cfg(windows)]
pub(crate) mod keys {
    use std::{
        io,
        os::windows::ffi::OsStrExt,
        sync::{
            atomic::{AtomicU64, Ordering},
            OnceLock,
        },
    };
    use windows_sys::Win32::{Foundation::*, System::Memory::*, UI::Input::KeyboardAndMouse::*};
    const KEYS: [u16; 4] = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN];
    pub struct Journal {
        handle: HANDLE,
        ptr: *mut AtomicU64,
        pub name: String,
    }
    unsafe impl Send for Journal {}
    unsafe impl Sync for Journal {}
    impl Journal {
        pub fn create() -> io::Result<Self> {
            Self::map(
                format!("Local\\AleExecutionKeys-{}", uuid::Uuid::new_v4()),
                true,
            )
        }
        fn map(name: String, create: bool) -> io::Result<Self> {
            if !name.starts_with("Local\\AleExecutionKeys-") || name.len() > 90 {
                return Err(io::Error::other("invalid key journal"));
            }
            let wide: Vec<u16> = std::ffi::OsStr::new(&name)
                .encode_wide()
                .chain(Some(0))
                .collect();
            let handle = unsafe {
                if create {
                    CreateFileMappingW(
                        INVALID_HANDLE_VALUE,
                        std::ptr::null(),
                        PAGE_READWRITE,
                        0,
                        8,
                        wide.as_ptr(),
                    )
                } else {
                    OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide.as_ptr())
                }
            };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let ptr = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 8) }
                .Value
                .cast::<AtomicU64>();
            if ptr.is_null() {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(io::Error::last_os_error());
            }
            if create {
                unsafe {
                    ptr.write(AtomicU64::new(0));
                }
            }
            Ok(Self { handle, ptr, name })
        }
        fn bits(&self) -> &AtomicU64 {
            unsafe { &*self.ptr }
        }
        pub fn cleanup(&self) {
            let mask = self.bits().swap(0, Ordering::AcqRel);
            let mut failed = 0;
            for (index, key) in KEYS.iter().enumerate() {
                if mask & (1 << index) != 0 {
                    let input = INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: *key,
                                wScan: 0,
                                dwFlags: KEYEVENTF_KEYUP,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    };
                    if unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) } != 1 {
                        failed += 1;
                    }
                }
            }
            ale_core::diagnostics::record(
                "automation_modifier_cleanup",
                &[("keys", mask.count_ones() as u64), ("failed", failed)],
            );
        }
    }
    impl Drop for Journal {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.ptr.cast(),
                });
                CloseHandle(self.handle);
            }
        }
    }
    static ACTIVE: OnceLock<Journal> = OnceLock::new();
    pub fn install(name: &str) -> io::Result<()> {
        let journal = Journal::map(name.into(), false)?;
        let _ = ACTIVE.set(journal);
        Ok(())
    }
    fn index(key: enigo::Key) -> Option<usize> {
        match key {
            enigo::Key::Control => Some(0),
            enigo::Key::Alt => Some(1),
            enigo::Key::Shift => Some(2),
            enigo::Key::Meta => Some(3),
            _ => None,
        }
    }
    pub fn pressing(key: enigo::Key) -> io::Result<()> {
        if let Some(journal) = ACTIVE.get() {
            let index = index(key).ok_or_else(|| io::Error::other("UNSUPPORTED_MODIFIER"))?;
            // Do not take ownership of a modifier the user already holds.
            if unsafe { GetAsyncKeyState(KEYS[index] as i32) } < 0 {
                return Err(io::Error::other("MODIFIER_ALREADY_HELD"));
            }
            journal.bits().fetch_or(1 << index, Ordering::AcqRel);
        }
        Ok(())
    }
    pub fn released(key: enigo::Key) {
        if let (Some(journal), Some(index)) = (ACTIVE.get(), index(key)) {
            journal.bits().fetch_and(!(1 << index), Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn helper_output_is_drained_with_a_fixed_limit() {
        let control = ExecutionControl::new(Instant::now() + Duration::from_secs(5));
        let output = bounded_output(
            Command::new("sh").args(["-c", "head -c 2097152 /dev/zero"]),
            &control,
        );
        assert_eq!(output.unwrap_err().to_string(), "HELPER_OUTPUT_LIMIT");
    }
    #[cfg(unix)]
    #[test]
    fn helper_hang_is_terminated_and_reaped() {
        let control = ExecutionControl::new(Instant::now() + Duration::from_millis(100));
        let started = Instant::now();
        let result = bounded_output(Command::new("sh").args(["-c", "sleep 60"]), &control);
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
