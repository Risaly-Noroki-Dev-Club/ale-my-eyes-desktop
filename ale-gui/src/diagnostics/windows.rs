use super::{events, Diagnostics};
use std::{
    ffi::c_void,
    fs::{self, File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    mem::{size_of, zeroed},
    os::windows::{ffi::OsStrExt, io::AsRawHandle, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, OnceLock,
    },
    time::{Duration, Instant},
};
use windows_sys::core::BOOL;
use windows_sys::Win32::{
    Foundation::*,
    System::{
        Diagnostics::{Debug::*, ProcessSnapshotting::*, ToolHelp::*},
        JobObjects::*,
        Memory::*,
        ProcessStatus::*,
        SystemInformation::GetTickCount64,
        Threading::*,
    },
    UI::WindowsAndMessaging::*,
};
const MAGIC: u64 = 0x414c454449414732;
const MAX_CAPTURE: Duration = Duration::from_secs(15);
const CAPTURE_COOLDOWN: Duration = Duration::from_secs(300);
const NO_WINDOW: u32 = 0x08000000;

#[repr(C)]
#[derive(Default)]
struct Shared {
    magic: AtomicU64,
    pid: AtomicU64,
    creation: AtomicU64,
    ui_tid: AtomicU64,
    started_tick: AtomicU64,
    mirror_tick: AtomicU64,
    heartbeat_tick: AtomicU64,
    heartbeat_count: AtomicU64,
    runtime_heartbeat_tick: AtomicU64,
    runtime_heartbeat_count: AtomicU64,
    fallback_active: AtomicU64,
    gui_events_dropped: AtomicU64,
    gui_log_failures: AtomicU64,
    gui_log_last_write: AtomicU64,
    flags: AtomicU64,
    downloaded: AtomicU64,
    stage: AtomicU64,
    stage_started_ms: AtomicU64,
    render: AtomicU64,
    crash_sequence: AtomicU64,
    crash_tid: AtomicU64,
    exception_pointer: AtomicU64,
    exception_code: AtomicU64,
    crash_kind: AtomicU64,
}
const READY: u64 = 1;
const BUSY: u64 = 2;
const CONNECTED: u64 = 4;
const DOWNLOADING: u64 = 8;
const DUMP_ENABLED: u64 = 16;
const SHUTDOWN: u64 = 32;
const RENDER_SUPPORTED: u64 = 64;
struct Handle(HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Handle {
    fn checked(value: HANDLE) -> io::Result<Self> {
        if value.is_null() || value == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(value))
        }
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Mapping {
    _handle: Handle,
    ptr: *mut Shared,
}
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}
impl Mapping {
    fn create(name: &str) -> io::Result<Self> {
        let handle = Handle::checked(unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                null(),
                PAGE_READWRITE,
                0,
                size_of::<Shared>() as u32,
                wide(name).as_ptr(),
            )
        })?;
        let ptr =
            unsafe { MapViewOfFile(handle.0, FILE_MAP_ALL_ACCESS, 0, 0, size_of::<Shared>()) }
                .Value
                .cast::<Shared>();
        if ptr.is_null() {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            ptr.write(Shared::default());
        }
        Ok(Self {
            _handle: handle,
            ptr,
        })
    }
    fn open(name: &str) -> io::Result<Self> {
        let handle = Handle::checked(unsafe {
            OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide(name).as_ptr())
        })?;
        let ptr =
            unsafe { MapViewOfFile(handle.0, FILE_MAP_ALL_ACCESS, 0, 0, size_of::<Shared>()) }
                .Value
                .cast::<Shared>();
        if ptr.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            _handle: handle,
            ptr,
        })
    }
    fn get(&self) -> &Shared {
        unsafe { &*self.ptr }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr.cast(),
            });
        }
    }
}
fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
fn event(name: &str) -> io::Result<Handle> {
    Handle::checked(unsafe { CreateEventW(null(), 1, 0, wide(name).as_ptr()) })
}
fn filetime(value: FILETIME) -> u64 {
    (value.dwHighDateTime as u64) << 32 | value.dwLowDateTime as u64
}
fn times(process: HANDLE) -> io::Result<(u64, u64, u64)> {
    let (mut creation, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    if unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((filetime(creation), filetime(kernel), filetime(user)))
}
static CRASH_SHARED: AtomicPtr<Shared> = AtomicPtr::new(null_mut());
static CRASH_EVENT: AtomicUsize = AtomicUsize::new(0);
static CAPTURE_ACK: AtomicUsize = AtomicUsize::new(0);
static CRASH_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static PREVIOUS_FILTER: AtomicUsize = AtomicUsize::new(0);
// Exception handlers can race normal teardown; these resources live until process termination.
static CRASH_RESOURCES: OnceLock<(Arc<Mapping>, Arc<Handle>, Arc<Handle>)> = OnceLock::new();
pub(super) fn fallback_active() -> bool {
    CRASH_RESOURCES
        .get()
        .is_some_and(|(m, _, _)| m.get().fallback_active.load(Ordering::Acquire) != 0)
}

pub(super) fn start(diagnostics: Arc<Diagnostics>) -> io::Result<()> {
    let name = format!("Local\\AleDiagnostics-{}", diagnostics.session);
    let mapping = Arc::new(Mapping::create(&name)?);
    let crash = Arc::new(event(&format!("{name}-crash"))?);
    let ack = Arc::new(event(&format!("{name}-ack"))?);
    let shared = mapping.get();
    let tick = unsafe { GetTickCount64() };
    shared
        .pid
        .store(std::process::id() as u64, Ordering::Relaxed);
    shared
        .creation
        .store(times(unsafe { GetCurrentProcess() })?.0, Ordering::Relaxed);
    shared
        .ui_tid
        .store(unsafe { GetCurrentThreadId() } as u64, Ordering::Relaxed);
    shared.started_tick.store(tick, Ordering::Relaxed);
    shared.mirror_tick.store(tick, Ordering::Relaxed);
    shared.heartbeat_tick.store(tick, Ordering::Relaxed);
    shared.runtime_heartbeat_tick.store(tick, Ordering::Relaxed);
    shared.magic.store(MAGIC, Ordering::Release);
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--diagnostic-helper", &name, &diagnostics.session])
        .arg(&diagnostics.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(NO_WINDOW);
    let mut child = command.spawn()?;
    let _ = CRASH_RESOURCES.set((mapping.clone(), crash.clone(), ack.clone()));
    CRASH_SHARED.store(mapping.ptr, Ordering::Release);
    CRASH_EVENT.store(crash.0 as usize, Ordering::Release);
    CAPTURE_ACK.store(ack.0 as usize, Ordering::Release);
    let previous = unsafe { SetUnhandledExceptionFilter(Some(unhandled_exception)) };
    PREVIOUS_FILTER.store(previous.map_or(0, |f| f as usize), Ordering::Release);
    std::thread::Builder::new()
        .name("ale-diagnostic-mirror".into())
        .spawn(move || {
            // These handles/mapping stay live while an exception hook may access them.
            let (_crash, _ack) = (crash, ack);
            let mut last_heartbeat = 0;
            let mut last_runtime_heartbeat = 0;
            loop {
                let state = &diagnostics.status;
                let shared = mapping.get();
                let tick = unsafe { GetTickCount64() };
                shared.mirror_tick.store(tick, Ordering::Relaxed);
                let count = state.heartbeat_count.load(Ordering::Acquire);
                if count != last_heartbeat {
                    shared.heartbeat_tick.store(tick, Ordering::Release);
                    last_heartbeat = count;
                }
                shared.heartbeat_count.store(count, Ordering::Relaxed);
                let runtime_count = state.runtime_heartbeat_count.load(Ordering::Acquire);
                if runtime_count != last_runtime_heartbeat {
                    shared.runtime_heartbeat_tick.store(tick, Ordering::Release);
                    last_runtime_heartbeat = runtime_count;
                }
                shared
                    .runtime_heartbeat_count
                    .store(runtime_count, Ordering::Relaxed);
                if let Some(sink) = events::current() {
                    shared.gui_events_dropped.store(
                        sink.health.dropped.load(Ordering::Relaxed),
                        Ordering::Relaxed,
                    );
                    shared.gui_log_failures.store(
                        sink.health.write_failures.load(Ordering::Relaxed),
                        Ordering::Relaxed,
                    );
                    shared.gui_log_last_write.store(
                        sink.health.last_write_unix_ms.load(Ordering::Relaxed),
                        Ordering::Relaxed,
                    );
                    if sink.health.fallback_active.load(Ordering::Acquire) {
                        shared.fallback_active.store(1, Ordering::Release);
                    }
                }
                let mut flags = 0;
                for (flag, value) in [
                    (READY, &state.ready),
                    (BUSY, &state.busy),
                    (CONNECTED, &state.connected),
                    (DOWNLOADING, &state.downloading),
                    (DUMP_ENABLED, &state.auto_dump),
                    (SHUTDOWN, &state.shutdown),
                    (RENDER_SUPPORTED, &state.render_notifier_supported),
                ] {
                    if value.load(Ordering::Acquire) {
                        flags |= flag;
                    }
                }
                shared.flags.store(flags, Ordering::Release);
                shared.downloaded.store(
                    state.downloaded_bytes.load(Ordering::Relaxed),
                    Ordering::Relaxed,
                );
                shared
                    .stage
                    .store(state.stage.load(Ordering::Relaxed), Ordering::Relaxed);
                shared.stage_started_ms.store(
                    state.stage_started_ms.load(Ordering::Relaxed),
                    Ordering::Relaxed,
                );
                shared.render.store(
                    state.render_state.load(Ordering::Relaxed),
                    Ordering::Relaxed,
                );
                if let Ok(Some(status)) = child.try_wait() {
                    events::record(
                        "diagnostic_helper_exited",
                        &[("success", u64::from(status.success()))],
                    );
                    // Retain the preallocated mapping until process termination; never leave hook dangling.
                    CRASH_EVENT.store(0, Ordering::Release);
                    CAPTURE_ACK.store(0, Ordering::Release);
                    CRASH_SHARED.store(null_mut(), Ordering::Release);
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })?;
    events::record("diagnostic_helper_started", &[]);
    Ok(())
}
fn notify_crash(kind: u64, pointers: *mut EXCEPTION_POINTERS) {
    let shared = CRASH_SHARED.load(Ordering::Acquire);
    let event = CRASH_EVENT.load(Ordering::Acquire) as HANDLE;
    let ack = CAPTURE_ACK.load(Ordering::Acquire) as HANDLE;
    if shared.is_null()
        || event.is_null()
        || ack.is_null()
        || CRASH_ACTIVE.swap(1, Ordering::AcqRel) != 0
    {
        return;
    }
    unsafe {
        let shared = &*shared;
        {
            let enabled = shared.flags.load(Ordering::Acquire) & DUMP_ENABLED != 0;
            ResetEvent(ack);
            shared
                .crash_tid
                .store(GetCurrentThreadId() as u64, Ordering::Relaxed);
            shared
                .exception_pointer
                .store(pointers as u64, Ordering::Relaxed);
            let code = if pointers.is_null() || (*pointers).ExceptionRecord.is_null() {
                0
            } else {
                (*(*pointers).ExceptionRecord).ExceptionCode as u32 as u64
            };
            shared.exception_code.store(code, Ordering::Relaxed);
            shared.crash_kind.store(kind, Ordering::Relaxed);
            shared.crash_sequence.fetch_add(1, Ordering::Release);
            SetEvent(event);
            // Bounded wait only: crash handlers must not wait forever or touch the logger.
            if enabled {
                WaitForSingleObject(ack, 10_000);
            }
        }
    }
    CRASH_ACTIVE.store(0, Ordering::Release);
}
pub(super) fn notify_panic() {
    notify_crash(2, null_mut());
}
unsafe extern "system" fn unhandled_exception(pointers: *const EXCEPTION_POINTERS) -> i32 {
    notify_crash(1, pointers.cast_mut());
    let previous = PREVIOUS_FILTER.load(Ordering::Acquire);
    if previous != 0 {
        let filter: unsafe extern "system" fn(*const EXCEPTION_POINTERS) -> i32 =
            std::mem::transmute(previous);
        return filter(pointers);
    }
    EXCEPTION_CONTINUE_SEARCH
}

pub(super) fn dispatch() -> Option<Result<(), String>> {
    let args: Vec<_> = std::env::args_os().collect();
    match args.get(1)?.to_str()? {
        "--diagnostic-helper" => Some((|| {
            let name = args
                .get(2)
                .and_then(|v| v.to_str())
                .ok_or("missing mapping")?;
            let session = args
                .get(3)
                .and_then(|v| v.to_str())
                .ok_or("missing session")?;
            let directory = args.get(4).ok_or("missing directory")?;
            helper(name, session, Path::new(directory)).map_err(|e| e.to_string())
        })()),
        "--diagnostic-capture" => Some((|| {
            let name = args
                .get(2)
                .and_then(|v| v.to_str())
                .ok_or("missing mapping")?;
            let path = args.get(3).ok_or("missing dump destination")?;
            if let Some(session) = name.strip_prefix("Local\\AleDiagnostics-") {
                let directory = Path::new(path).parent().ok_or("invalid destination")?;
                let _ = events::install(directory.into(), session.into(), "diagnostic-capture");
            }
            let result = capture(name, Path::new(path));
            if let Err(error) = &result {
                events::record(
                    "capture_error",
                    &[("os_error", error.raw_os_error().unwrap_or(0) as u64)],
                );
            }
            if let Some(sink) = events::current() {
                let _ = sink.flush(Duration::from_millis(500));
            }
            result.map_err(|e| e.to_string())
        })()),
        _ => None,
    }
}
struct Probe {
    pid: u32,
    hwnd: HWND,
}
unsafe extern "system" fn find_window(hwnd: HWND, param: LPARAM) -> BOOL {
    let probe = &mut *(param as *mut Probe);
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == probe.pid && IsWindowVisible(hwnd) != 0 && GetWindow(hwnd, GW_OWNER).is_null() {
        probe.hwnd = hwnd;
        return 0;
    }
    1
}
fn window_responds(pid: u32) -> Option<bool> {
    let mut probe = Probe {
        pid,
        hwnd: null_mut(),
    };
    unsafe {
        EnumWindows(Some(find_window), &mut probe as *mut Probe as LPARAM);
    }
    if probe.hwnd.is_null() {
        return None;
    }
    let mut result = 0;
    Some(
        unsafe {
            SendMessageTimeoutW(
                probe.hwnd,
                WM_NULL,
                0,
                0,
                SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
                500,
                &mut result,
            )
        } != 0,
    )
}
fn thread_count(pid: u32) -> u64 {
    let Ok(snapshot) = Handle::checked(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })
    else {
        return 0;
    };
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut count = 0;
    let mut success = unsafe { Thread32First(snapshot.0, &mut entry) };
    while success != 0 {
        if entry.th32OwnerProcessID == pid {
            count += 1;
        }
        success = unsafe { Thread32Next(snapshot.0, &mut entry) };
    }
    count
}
fn resources(process: HANDLE, pid: u32) -> serde_json::Value {
    let mut memory: PROCESS_MEMORY_COUNTERS_EX = unsafe { zeroed() };
    memory.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let memory_ok = unsafe {
        K32GetProcessMemoryInfo(
            process,
            &mut memory as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    } != 0;
    let mut handles = 0;
    let handles_ok = unsafe { GetProcessHandleCount(process, &mut handles) } != 0;
    let times = times(process).ok();
    serde_json::json!({"kernel_100ns":times.map(|t|t.1),"user_100ns":times.map(|t|t.2),"working_set_bytes":memory_ok.then_some(memory.WorkingSetSize),"private_bytes":memory_ok.then_some(memory.PrivateUsage),"handle_count":handles_ok.then_some(handles),"thread_count":thread_count(pid)})
}
enum FileTask {
    Snapshot(serde_json::Value),
    RemovePartial(PathBuf),
}
struct CaptureChild {
    child: Child,
    started: Instant,
    output: PathBuf,
    crash_sequence: u64,
    timed_out: bool,
}
fn helper(name: &str, session: &str, directory: &Path) -> io::Result<()> {
    let mapping = Mapping::open(name)?;
    let shared = mapping.get();
    if shared.magic.load(Ordering::Acquire) != MAGIC {
        return Err(io::Error::other("diagnostic schema mismatch"));
    }
    let pid = shared.pid.load(Ordering::Relaxed) as u32;
    let process = Handle::checked(unsafe {
        OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    })?;
    if times(process.0)?.0 != shared.creation.load(Ordering::Relaxed) {
        return Err(io::Error::other("target process identity mismatch"));
    }
    let _crash = event(&format!("{name}-crash"))?;
    let ack = event(&format!("{name}-ack"))?;
    let _ = events::install(directory.into(), session.into(), "diagnostic-helper");
    events::record(
        "monitor_started",
        &[
            ("target_pid", pid as u64),
            ("ui_tid", shared.ui_tid.load(Ordering::Acquire)),
        ],
    );
    events::record(
        "display_environment",
        &[
            ("monitors", unsafe { GetSystemMetrics(SM_CMONITORS) } as u64),
            (
                "remote_session",
                unsafe { GetSystemMetrics(SM_REMOTESESSION) } as u64,
            ),
        ],
    );
    // A single bounded file writer: a slow Desktop cannot stop probes or spawn unbounded writers.
    let (writer, receiver) = mpsc::sync_channel(4);
    let root = directory.to_path_buf();
    let writer_mapping = Mapping::open(name)?;
    let writer_session = session.to_string();
    std::thread::Builder::new()
        .name("ale-snapshot-writer".into())
        .spawn(move || {
            while let Ok(task) = receiver.recv() {
                let root = if writer_mapping.get().fallback_active.load(Ordering::Acquire) != 0 {
                    events::fallback_directory()
                } else {
                    root.clone()
                };
                match task {
                    FileTask::Snapshot(snapshot) => {
                        if super::write_snapshot(&root, &writer_session, snapshot).is_err() {
                            events::record("snapshot_write_failed", &[]);
                        }
                    }
                    FileTask::RemovePartial(path) => {
                        let _ = fs::remove_file(path);
                    }
                }
                if events::current()
                    .is_some_and(|sink| sink.health.fallback_active.load(Ordering::Acquire))
                {
                    writer_mapping
                        .get()
                        .fallback_active
                        .store(1, Ordering::Release);
                }
                let _ = events::prune(&root);
            }
        })?;
    let mut hang_detector = super::HangDetector::default();
    let mut incident = false;
    let mut last_capture: Option<Instant> = None;
    let mut capture_child: Option<CaptureChild> = None;
    let mut seen_crash = 0;
    let mut next_snapshot = Instant::now();
    loop {
        let tick = unsafe { GetTickCount64() };
        let flags = shared.flags.load(Ordering::Acquire);
        let exited = unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0;
        let response = if exited { None } else { window_responds(pid) };
        let heartbeat_age = tick.saturating_sub(shared.heartbeat_tick.load(Ordering::Acquire));
        let heartbeat_count = shared.heartbeat_count.load(Ordering::Relaxed);
        let hung = !exited
            && hang_detector.update(
                tick,
                response,
                heartbeat_count,
                heartbeat_age,
                flags & SHUTDOWN != 0,
            );
        let crash_sequence = shared.crash_sequence.load(Ordering::Acquire);
        let new_crash = crash_sequence > seen_crash;
        if new_crash {
            seen_crash = crash_sequence;
            events::record(
                "crash_notification",
                &[
                    ("target_tid", shared.crash_tid.load(Ordering::Relaxed)),
                    (
                        "exception_code",
                        shared.exception_code.load(Ordering::Relaxed),
                    ),
                    ("kind", shared.crash_kind.load(Ordering::Relaxed)),
                ],
            );
        }
        let new_hang = hung && !incident;
        if new_hang {
            incident = true;
            events::record(
                "ui_hang_detected",
                &[
                    ("ui_tid", shared.ui_tid.load(Ordering::Relaxed)),
                    ("heartbeat_age_ms", heartbeat_age),
                ],
            );
        }
        if incident && !hung && !exited {
            incident = false;
            events::record("ui_recovered", &[]);
        }
        if (new_hang || new_crash)
            && flags & DUMP_ENABLED != 0
            && capture_child.is_none()
            && (new_crash || last_capture.is_none_or(|last| last.elapsed() >= CAPTURE_COOLDOWN))
        {
            let prefix = if new_crash { "ale-crash" } else { "ale-hang" };
            let destination = if shared.fallback_active.load(Ordering::Acquire) != 0 {
                events::fallback_directory()
            } else {
                directory.to_path_buf()
            };
            let output = destination.join(format!(
                "{prefix}-{}-{session}-{pid}.dmp",
                events::unix_ms()
            ));
            let spawn = Command::new(std::env::current_exe()?)
                .arg("--diagnostic-capture")
                .arg(name)
                .arg(&output)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(NO_WINDOW)
                .spawn();
            match spawn {
                Ok(child) => {
                    capture_child = Some(CaptureChild {
                        child,
                        started: Instant::now(),
                        output,
                        crash_sequence: if new_crash { crash_sequence } else { 0 },
                        timed_out: false,
                    });
                    last_capture = Some(Instant::now());
                    events::record("dump_started", &[]);
                }
                Err(error) => {
                    events::record(
                        "dump_spawn_failed",
                        &[("os_error", error.raw_os_error().unwrap_or(0) as u64)],
                    );
                    unsafe {
                        SetEvent(ack.0);
                    }
                }
            }
        } else if new_crash {
            unsafe {
                SetEvent(ack.0);
            }
        }
        if let Some(worker) = &mut capture_child {
            if !worker.timed_out && worker.started.elapsed() >= MAX_CAPTURE {
                worker.timed_out = true;
                let _ = worker.child.kill();
                events::record("dump_timeout", &[]);
                unsafe {
                    SetEvent(ack.0);
                }
            }
            // Never block the independent monitor in Child::wait. Retain the
            // only capture slot until the process is observed to have exited.
            if let Ok(Some(status)) = worker.child.try_wait() {
                let success = status.success() && !worker.timed_out;
                events::record(
                    if success {
                        "dump_completed"
                    } else {
                        "dump_failed"
                    },
                    &[],
                );
                if !success {
                    let _ = writer.try_send(FileTask::RemovePartial(
                        worker.output.with_extension("dmp.partial"),
                    ));
                }
                if worker.crash_sequence != 0 {
                    unsafe {
                        SetEvent(ack.0);
                    }
                }
                capture_child = None;
            }
        }

        let target_resources = resources(process.0, pid);
        let helper_resources = resources(unsafe { GetCurrentProcess() }, std::process::id());
        if Instant::now() >= next_snapshot || new_hang || new_crash || exited {
            let sink = events::current();
            let snapshot = serde_json::json!({"schema_version":2,"timestamp_unix_ms":events::unix_ms(),"session":session,"pid":pid,"version":env!("CARGO_PKG_VERSION"),"os":"windows","uptime_ms":tick.saturating_sub(shared.started_tick.load(Ordering::Relaxed)),"ui_thread_id":shared.ui_tid.load(Ordering::Relaxed),"ui_heartbeat_age_ms":heartbeat_age,"ui_heartbeat_count":heartbeat_count,"runtime_heartbeat_age_ms":tick.saturating_sub(shared.runtime_heartbeat_tick.load(Ordering::Acquire)),"runtime_heartbeat_count":shared.runtime_heartbeat_count.load(Ordering::Relaxed),"gui_events_dropped":shared.gui_events_dropped.load(Ordering::Relaxed),"gui_log_write_failures":shared.gui_log_failures.load(Ordering::Relaxed),"gui_log_last_write_unix_ms":shared.gui_log_last_write.load(Ordering::Relaxed),"ui_unresponsive":hung,"window_message_responding":response,"independent_windows_monitor":true,"ready":flags&READY!=0,"busy":flags&BUSY!=0,"connected":flags&CONNECTED!=0,"downloading":flags&DOWNLOADING!=0,"downloaded_bytes":shared.downloaded.load(Ordering::Relaxed),"auto_dump":flags&DUMP_ENABLED!=0,"stage_id":shared.stage.load(Ordering::Relaxed),"stage_started_ms":shared.stage_started_ms.load(Ordering::Relaxed),"render_notifier_supported":flags&RENDER_SUPPORTED!=0,"render_phase_before_present":shared.render.load(Ordering::Relaxed),"mirror_age_ms":tick.saturating_sub(shared.mirror_tick.load(Ordering::Relaxed)),"process_exited":exited,"normal_shutdown":flags&SHUTDOWN!=0,"resources":target_resources,"helper_resources":helper_resources,"events_dropped":sink.map(|s|s.health.dropped.load(Ordering::Relaxed)),"log_write_failures":sink.map(|s|s.health.write_failures.load(Ordering::Relaxed)),"log_last_write_unix_ms":sink.map(|s|s.health.last_write_unix_ms.load(Ordering::Relaxed))});
            if writer.try_send(FileTask::Snapshot(snapshot)).is_err() {
                events::record("snapshot_queue_full", &[]);
            }
            next_snapshot = Instant::now() + Duration::from_secs(60);
        }
        if exited && capture_child.is_none() {
            events::record(
                if flags & SHUTDOWN != 0 {
                    "target_exit_normal"
                } else {
                    "target_exit_unexpected"
                },
                &[],
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(
            if new_crash || capture_child.is_some() {
                100
            } else {
                1000
            },
        ));
    }
    // Bound final flushing; no GUI dependency and no re-spawn after target exit.
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}

struct Snapshot {
    snapshot: HPSS,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        // PssCaptureSnapshot creates the descriptor in this capture process,
        // even though its contents come from the monitored GUI process.
        let error = unsafe { PssFreeSnapshot(GetCurrentProcess(), self.snapshot) };
        if error != 0 {
            events::record("snapshot_release_failed", &[("os_error", error as u64)]);
        }
    }
}
struct DumpIo {
    file: File,
    started: Instant,
    limited: bool,
}
#[allow(non_upper_case_globals)]
unsafe extern "system" fn dump_callback(
    context: *mut c_void,
    input: *const MINIDUMP_CALLBACK_INPUT,
    output: *mut MINIDUMP_CALLBACK_OUTPUT,
) -> BOOL {
    if input.is_null() || output.is_null() {
        return 0;
    }
    let context = &mut *context.cast::<DumpIo>();
    let callback_type = (*input).CallbackType as i32;
    match callback_type {
        IsProcessSnapshotCallback => {
            (*output).Anonymous.Status = 1;
        }
        IoStartCallback => {
            (*output).Anonymous.Status = 1;
        }
        IoWriteAllCallback => {
            let io = (*input).Anonymous.Io;
            if io.Offset.saturating_add(io.BufferBytes as u64) > events::MAX_DUMP_BYTES
                || context.started.elapsed() >= Duration::from_secs(12)
            {
                context.limited = true;
                (*output).Anonymous.Status = 0x80004004u32 as i32;
                return 0;
            }
            let result = context.file.seek(SeekFrom::Start(io.Offset)).and_then(|_| {
                context.file.write_all(std::slice::from_raw_parts(
                    io.Buffer.cast::<u8>(),
                    io.BufferBytes as usize,
                ))
            });
            (*output).Anonymous.Status = if result.is_ok() {
                0
            } else {
                0x80004005u32 as i32
            };
            if result.is_err() {
                return 0;
            }
        }
        IoFinishCallback => {
            (*output).Anonymous.Status = 0;
        }
        CancelCallback => {
            (*output).Anonymous.Anonymous2 = MINIDUMP_CALLBACK_OUTPUT_0_1 {
                CheckCancel: 1,
                Cancel: i32::from(context.started.elapsed() >= Duration::from_secs(12)),
            };
        }
        _ => {}
    }
    1
}
fn capture(name: &str, destination: &Path) -> io::Result<()> {
    let mapping = Mapping::open(name)?;
    let shared = mapping.get();
    if shared.magic.load(Ordering::Acquire) != MAGIC {
        return Err(io::Error::other("diagnostic schema mismatch"));
    }
    let pid = shared.pid.load(Ordering::Relaxed) as u32;
    let process = Handle::checked(unsafe {
        OpenProcess(
            PROCESS_QUERY_INFORMATION
                | PROCESS_VM_READ
                | PROCESS_CREATE_PROCESS
                | PROCESS_DUP_HANDLE,
            0,
            pid,
        )
    })?;
    if times(process.0)?.0 != shared.creation.load(Ordering::Relaxed) {
        return Err(io::Error::other("process identity changed"));
    }
    #[cfg(target_arch = "x86_64")]
    let context_flags = CONTEXT_ALL_AMD64;
    #[cfg(target_arch = "x86")]
    let context_flags = CONTEXT_ALL_X86;
    #[cfg(target_arch = "aarch64")]
    let context_flags = CONTEXT_ALL_ARM64;
    let mut snapshot_handle = null_mut();
    let error = unsafe {
        PssCaptureSnapshot(
            process.0,
            PSS_CAPTURE_VA_CLONE | PSS_CAPTURE_THREADS | PSS_CAPTURE_THREAD_CONTEXT,
            context_flags,
            &mut snapshot_handle,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let snapshot = Snapshot {
        snapshot: snapshot_handle,
    };
    let mut clone_info: PSS_VA_CLONE_INFORMATION = unsafe { zeroed() };
    let query = unsafe {
        PssQuerySnapshot(
            snapshot.snapshot,
            PSS_QUERY_VA_CLONE_INFORMATION,
            &mut clone_info as *mut _ as *mut c_void,
            size_of::<PSS_VA_CLONE_INFORMATION>() as u32,
        )
    };
    if query != 0 {
        return Err(io::Error::from_raw_os_error(query as i32));
    }
    let clone_job = Handle::checked(unsafe { CreateJobObjectW(null(), null()) })?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            clone_job.0,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { AssignProcessToJobObject(clone_job.0, clone_info.VaCloneHandle) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("missing dump directory"))?;
    fs::create_dir_all(parent)?;
    events::prune(parent)?;
    let temporary = destination.with_extension("dmp.partial");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let file_handle = file.as_raw_handle();
    let mut context = DumpIo {
        file,
        started: Instant::now(),
        limited: false,
    };
    let callback = MINIDUMP_CALLBACK_INFORMATION {
        CallbackRoutine: Some(dump_callback),
        CallbackParam: &mut context as *mut DumpIo as *mut c_void,
    };
    let exception_pointer = shared.exception_pointer.load(Ordering::Acquire);
    let exception = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: shared.crash_tid.load(Ordering::Relaxed) as u32,
        ExceptionPointers: exception_pointer as *mut EXCEPTION_POINTERS,
        ClientPointers: 1,
    };
    let success = unsafe {
        MiniDumpWriteDump(
            snapshot.snapshot,
            pid,
            file_handle,
            MiniDumpNormal | MiniDumpWithThreadInfo,
            if exception_pointer == 0 {
                null()
            } else {
                &exception
            },
            null(),
            &callback,
        )
    } != 0;
    let error = io::Error::last_os_error();
    context.file.flush()?;
    let bytes = context.file.metadata()?.len();
    drop(context);
    drop(snapshot);
    if !success || bytes == 0 || bytes > events::MAX_DUMP_BYTES {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, destination)?;
    events::register_file(destination)?;
    let sidecar = serde_json::json!({"schema_version":2,"pid":pid,"creation_time":shared.creation.load(Ordering::Relaxed),"ui_thread_id":shared.ui_tid.load(Ordering::Relaxed),"exception_thread_id":shared.crash_tid.load(Ordering::Relaxed),"exception_code":shared.exception_code.load(Ordering::Relaxed),"dump_bytes":bytes,"type":"MiniDumpNormal|MiniDumpWithThreadInfo","snapshot":"PSS","contains_potentially_sensitive_stack_memory":true,"build":events::build_info(),"version":env!("CARGO_PKG_VERSION")});
    fs::write(
        destination.with_extension("json"),
        serde_json::to_vec_pretty(&sidecar)?,
    )?;
    events::register_file(&destination.with_extension("json"))?;
    events::prune(parent)?;
    Ok(())
}
