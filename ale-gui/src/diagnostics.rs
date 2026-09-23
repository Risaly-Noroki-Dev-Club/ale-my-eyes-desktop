//! Early, bounded diagnostics; Windows monitoring and dump writing live in other processes.
use ale_core::diagnostics as events;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};
#[cfg(windows)]
mod windows;

#[repr(C)]
#[derive(Default)]
pub struct Status {
    pub heartbeat_ms: AtomicU64,
    pub heartbeat_count: AtomicU64,
    pub runtime_heartbeat_ms: AtomicU64,
    pub runtime_heartbeat_count: AtomicU64,
    pub busy: AtomicBool,
    pub ready: AtomicBool,
    pub connected: AtomicBool,
    pub downloading: AtomicBool,
    pub downloaded_bytes: AtomicU64,
    pub stage: AtomicU64,
    pub stage_started_ms: AtomicU64,
    pub render_state: AtomicU64,
    pub render_notifier_supported: AtomicBool,
    pub auto_dump: AtomicBool,
    pub settings_loaded: AtomicBool,
    pub settings_generation: AtomicU64,
    pub settings_flushed: AtomicBool,
    pub shutdown: AtomicBool,
}

pub struct Diagnostics {
    pub status: Arc<Status>,
    pub directory: PathBuf,
    fallback_directory: PathBuf,
    pub session: String,
    started: Instant,
    ui_thread: std::thread::ThreadId,
}
static CURRENT: OnceLock<Arc<Diagnostics>> = OnceLock::new();

pub fn current() -> Option<Arc<Diagnostics>> {
    CURRENT.get().cloned()
}
pub fn bootstrap() -> Arc<Diagnostics> {
    CURRENT
        .get_or_init(|| {
            let desktop = dirs::desktop_dir()
                .or_else(dirs::data_local_dir)
                .unwrap_or_else(std::env::temp_dir);
            let directory = std::env::var_os("ALE_DIAGNOSTICS_DIRECTORY")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| desktop.join("Ale-My-Eyes-Logs"));
            let session = uuid::Uuid::new_v4().to_string();
            let state = Arc::new(Diagnostics {
                status: Arc::new(Status::default()),
                directory: directory.clone(),
                // Resolve known folders before the window starts. On Windows this
                // can verify a redirected directory through SHGetKnownFolderPath.
                fallback_directory: events::fallback_directory(),
                session: session.clone(),
                started: Instant::now(),
                ui_thread: std::thread::current().id(),
            });
            let _ = events::install(directory, session, "gui");
            events::record("process_start", &[]);
            state.start_settings_worker();
            #[cfg(windows)]
            if let Err(error) = windows::start(state.clone()) {
                events::record(
                    "helper_start_failed",
                    &[("os_error", error.raw_os_error().unwrap_or(0) as u64)],
                );
                state.start_snapshot_writer(Duration::from_secs(60));
            }
            #[cfg(not(windows))]
            state.start_snapshot_writer(Duration::from_secs(60));
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                events::record(
                    "rust_panic",
                    &[("line", info.location().map_or(0, |l| l.line() as u64))],
                );
                #[cfg(windows)]
                windows::notify_panic();
                previous(info);
            }));
            state
        })
        .clone()
}

impl Diagnostics {
    /// Compatibility entry point. Prefer bootstrap before constructing the runtime or window.
    pub fn start() -> std::io::Result<Arc<Self>> {
        Ok(bootstrap())
    }
    pub fn heartbeat(&self, busy: bool, ready: bool, connected: bool) {
        self.status
            .heartbeat_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Release);
        self.status.heartbeat_count.fetch_add(1, Ordering::Relaxed);
        self.status.busy.store(busy, Ordering::Relaxed);
        self.status.ready.store(ready, Ordering::Relaxed);
        self.status.connected.store(connected, Ordering::Relaxed);
    }
    pub fn runtime_heartbeat(&self) {
        self.status
            .runtime_heartbeat_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Release);
        self.status
            .runtime_heartbeat_count
            .fetch_add(1, Ordering::Relaxed);
    }
    /// Reads only atomic state; opening a slow Desktop never blocks the UI here.
    pub fn actual_directory(&self) -> PathBuf {
        let fallback = events::current()
            .is_some_and(|sink| sink.health.fallback_active.load(Ordering::Acquire));
        #[cfg(windows)]
        let fallback = fallback || windows::fallback_active();
        self.directory_for_fallback(fallback)
    }
    fn directory_for_fallback(&self, fallback: bool) -> PathBuf {
        if fallback {
            self.fallback_directory.clone()
        } else {
            self.directory.clone()
        }
    }
    fn start_settings_worker(self: &Arc<Self>) {
        let this = self.clone();
        let _ = std::thread::Builder::new().name("ale-diagnostic-settings".into()).spawn(move || {
            let path = dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("ale-my-eyes/diagnostics-settings.json");
            let initial_generation = this.status.settings_generation.load(Ordering::Acquire);
            let enabled = std::fs::File::open(&path).ok().and_then(|file| { use std::io::Read; let mut bytes = Vec::new(); file.take(4097).read_to_end(&mut bytes).ok()?; (bytes.len() <= 4096).then_some(bytes) }).and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok()).and_then(|value| value["auto_dump"].as_bool()).unwrap_or(true);
            if this.status.settings_generation.load(Ordering::Acquire) == initial_generation {
                this.status.auto_dump.store(enabled, Ordering::Release);
            }
            this.status.settings_loaded.store(true, Ordering::Release);
            let mut saved_generation = initial_generation;
            let mut retry_after = Instant::now();
            loop {
                let shutting_down = this.status.shutdown.load(Ordering::Acquire);
                let generation = this.status.settings_generation.load(Ordering::Acquire);
                if generation != saved_generation && (shutting_down || Instant::now() >= retry_after) {
                    let result = (|| -> std::io::Result<()> {
                        std::fs::create_dir_all(path.parent().unwrap())?;
                        let bytes = serde_json::to_vec(&serde_json::json!({"auto_dump": this.status.auto_dump.load(Ordering::Acquire)}))?;
                        let temporary = path.with_extension("json.partial");
                        std::fs::write(&temporary, bytes)?;
                        // Settings contain no credentials; use replacement to support Windows.
                        #[cfg(windows)]
                        { use std::os::windows::ffi::OsStrExt; use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
                            let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
                            let destination: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
                            if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 { return Err(std::io::Error::last_os_error()); }
                        }
                        #[cfg(not(windows))] std::fs::rename(temporary, &path)?;
                        Ok(())
                    })();
                    events::record(if result.is_ok() {"diagnostic_settings_saved"} else {"diagnostic_settings_write_failed"}, &[]);
                    if result.is_ok() { saved_generation = generation; } else { retry_after = Instant::now() + Duration::from_secs(60); }
                }
                if shutting_down { this.status.settings_flushed.store(true, Ordering::Release); break; }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
    }
    fn start_snapshot_writer(self: &Arc<Self>, interval: Duration) {
        let this = self.clone();
        let _ = std::thread::Builder::new().name("ale-minute-logs".into()).spawn(move || {
            let mut next = Instant::now();
            while !this.status.shutdown.load(Ordering::Acquire) {
                if Instant::now() >= next {
                    let elapsed = this.started.elapsed().as_millis() as u64;
                    let heartbeat_count = this.status.heartbeat_count.load(Ordering::Relaxed);
                    let age = elapsed.saturating_sub(this.status.heartbeat_ms.load(Ordering::Acquire));
                    let snapshot = serde_json::json!({"schema_version":2,"timestamp_unix_ms":events::unix_ms(),"session":this.session,"pid":std::process::id(),"version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"uptime_ms":elapsed,"ui_heartbeat_age_ms":age,"ui_heartbeat_count":heartbeat_count,"runtime_heartbeat_age_ms":elapsed.saturating_sub(this.status.runtime_heartbeat_ms.load(Ordering::Acquire)),"runtime_heartbeat_count":this.status.runtime_heartbeat_count.load(Ordering::Relaxed),"events_dropped":events::current().map(|s|s.health.dropped.load(Ordering::Relaxed)),"log_write_failures":events::current().map(|s|s.health.write_failures.load(Ordering::Relaxed)),"log_last_write_unix_ms":events::current().map(|s|s.health.last_write_unix_ms.load(Ordering::Relaxed)),"ui_unresponsive":heartbeat_count > 0 && age >= 5000,"ready":this.status.ready.load(Ordering::Relaxed),"busy":this.status.busy.load(Ordering::Relaxed),"connected":this.status.connected.load(Ordering::Relaxed),"downloading":this.status.downloading.load(Ordering::Relaxed),"downloaded_bytes":this.status.downloaded_bytes.load(Ordering::Relaxed),"stage_id":this.status.stage.load(Ordering::Relaxed),"independent_windows_monitor":false,"auto_dump":this.status.auto_dump.load(Ordering::Acquire)});
                    if write_snapshot(&this.actual_directory(), &this.session, snapshot).is_err() { events::record("snapshot_write_failed", &[]); }
                    let _ = events::prune(&this.actual_directory());
                    next = Instant::now() + interval;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    }
}

pub fn auto_dump_enabled() -> bool {
    current().is_some_and(|d| d.status.auto_dump.load(Ordering::Acquire))
}
pub fn set_auto_dump_enabled(enabled: bool) {
    if let Some(d) = current() {
        d.status.auto_dump.store(enabled, Ordering::Release);
        d.status.settings_generation.fetch_add(1, Ordering::Release);
        events::record("auto_dump_changed", &[("enabled", u64::from(enabled))]);
    }
}
pub fn shutdown_requested() {
    events::record("normal_shutdown", &[]);
    if let Some(d) = current() {
        d.status.shutdown.store(true, Ordering::Release);
        // This runs after app.run returned. A slow preference disk cannot delay
        // closing indefinitely, but the worker gets a final chance to persist.
        let until = Instant::now() + Duration::from_secs(1);
        while !d.status.settings_flushed.load(Ordering::Acquire) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if let Some(sink) = events::current() {
        let _ = sink.flush(Duration::from_millis(500));
    }
}

pub struct StageGuard {
    diagnostics: Option<Arc<Diagnostics>>,
    id: u64,
    operation: u64,
    previous: u64,
    previous_started: u64,
    started: Instant,
}
fn stage_id(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    })
}
pub fn stage(name: &'static str) -> StageGuard {
    let d = current().filter(|d| d.ui_thread == std::thread::current().id());
    let id = stage_id(name);
    static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);
    let operation = NEXT_OPERATION.fetch_add(1, Ordering::Relaxed);
    let mut previous = 0;
    let mut previous_started = 0;
    if let Some(d) = &d {
        previous = d.status.stage.swap(id, Ordering::AcqRel);
        previous_started = d
            .status
            .stage_started_ms
            .swap(d.started.elapsed().as_millis() as u64, Ordering::AcqRel);
    }
    // The event literal identifies the phase without storing application text.
    events::record(
        name,
        &[("operation_id", operation), ("phase", 1), ("stage_id", id)],
    );
    StageGuard {
        diagnostics: d,
        id,
        operation,
        previous,
        previous_started,
        started: Instant::now(),
    }
}
impl Drop for StageGuard {
    fn drop(&mut self) {
        if let Some(d) = &self.diagnostics {
            if d.status
                .stage
                .compare_exchange(self.id, self.previous, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                d.status
                    .stage_started_ms
                    .store(self.previous_started, Ordering::Release);
            }
        }
        events::record(
            "stage_completed",
            &[
                ("operation_id", self.operation),
                ("stage_id", self.id),
                ("elapsed_ms", self.started.elapsed().as_millis() as u64),
            ],
        );
    }
}
pub fn render_notifier_supported(supported: bool) {
    if let Some(d) = current() {
        d.status
            .render_notifier_supported
            .store(supported, Ordering::Relaxed);
    }
    events::record(
        "render_notifier_support",
        &[("supported", u64::from(supported))],
    );
}
pub fn rendering_state(state: u64) {
    if let Some(d) = current() {
        d.status.render_state.store(state, Ordering::Relaxed);
    }
}
pub fn dispatch_internal_mode() -> Option<Result<(), String>> {
    #[cfg(windows)]
    {
        windows::dispatch()
    }
    #[cfg(not(windows))]
    {
        None
    }
}
fn write_snapshot(
    directory: &std::path::Path,
    session: &str,
    mut value: serde_json::Value,
) -> std::io::Result<()> {
    value["build"] = events::build_info();
    value["writer_tid"] = serde_json::json!(events::thread_id());
    let result = write_snapshot_at(directory, session, &value);
    if result.is_ok() || directory == events::fallback_directory() {
        return result;
    }
    if let Some(sink) = events::current() {
        sink.health.write_failures.fetch_add(1, Ordering::Relaxed);
        sink.health.fallback_active.store(true, Ordering::Release);
    }
    write_snapshot_at(&events::fallback_directory(), session, &value)
}
fn write_snapshot_at(
    directory: &std::path::Path,
    session: &str,
    value: &serde_json::Value,
) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    let name = format!(
        "ale-diagnostic-{}-{}-{session}.json",
        events::unix_ms(),
        std::process::id()
    );
    let partial = directory.join(format!("{name}.partial"));
    std::fs::write(&partial, serde_json::to_vec_pretty(&value)?)?;
    let destination = directory.join(name);
    std::fs::rename(partial, &destination)?;
    events::register_file(&destination)
}

/// Monitor timing depends on window messages, never on rendering callbacks.
#[cfg(any(windows, test))]
#[derive(Default)]
pub(super) struct HangDetector {
    first_failed_ms: Option<u64>,
    failures: u32,
    last_probe_ms: Option<u64>,
    resume_grace_until: u64,
}
#[cfg(any(windows, test))]
impl HangDetector {
    pub fn update(
        &mut self,
        now: u64,
        response: Option<bool>,
        heartbeat_count: u64,
        heartbeat_age: u64,
        shutdown: bool,
    ) -> bool {
        if self
            .last_probe_ms
            .is_some_and(|last| now.saturating_sub(last) > 6000)
        {
            self.first_failed_ms = None;
            self.failures = 0;
            self.resume_grace_until = now.saturating_add(5000);
        }
        self.last_probe_ms = Some(now);
        if response == Some(false) {
            self.first_failed_ms.get_or_insert(now);
            self.failures = self.failures.saturating_add(1);
        } else {
            self.first_failed_ms = None;
            self.failures = 0;
        }
        !shutdown
            && now >= self.resume_grace_until
            && ((self.failures >= 3
                && self
                    .first_failed_ms
                    .is_some_and(|start| now.saturating_sub(start) >= 5000))
                || (response.is_some() && heartbeat_count > 0 && heartbeat_age >= 10_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fallback_selection_uses_the_resolved_directory_without_filesystem_access() {
        // Deliberately use paths unrelated to the user's actual known folders.
        // Neither path needs to exist, and switching back must preserve both.
        let directory = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let fallback_directory = directory.join("resolved-fallback");
        let diagnostics = Diagnostics {
            status: Arc::new(Status::default()),
            directory: directory.clone(),
            fallback_directory: fallback_directory.clone(),
            session: "test".into(),
            started: Instant::now(),
            ui_thread: std::thread::current().id(),
        };
        for fallback in [false, true, true, false] {
            assert_eq!(
                diagnostics.directory_for_fallback(fallback),
                if fallback {
                    &fallback_directory
                } else {
                    &directory
                }
                .clone()
            );
        }
        assert!(!directory.exists());
    }

    #[test]
    fn snapshots_are_v2_and_atomic() {
        let directory = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        write_snapshot(&directory, "test", serde_json::json!({"schema_version":2})).unwrap();
        let entries: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .filter(|entry| {
                entry.as_ref().is_ok_and(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("ale-diagnostic-")
                })
            })
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(entries[0]
            .as_ref()
            .unwrap()
            .path()
            .extension()
            .is_some_and(|s| s == "json"));
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn monitor_works_without_callbacks_and_does_not_confuse_sleep_with_hang() {
        let mut detector = HangDetector::default();
        for now in [0, 1000, 2000, 3000, 4000] {
            assert!(!detector.update(now, Some(false), 0, 0, false));
        }
        assert!(detector.update(5000, Some(false), 0, 0, false));
        assert!(!detector.update(6000, Some(true), 0, 0, false));
        assert!(!detector.update(60_000, Some(true), 2, 50_000, false));
        assert!(!detector.update(61_000, Some(true), 3, 100, false));
        assert!(!detector.update(70_000, None, 0, 0, false));
        assert!(!detector.update(71_000, Some(false), 3, 60_000, true));
    }

    #[test]
    fn stage_identifiers_are_stable_and_distinct() {
        assert_eq!(stage_id("config"), stage_id("config"));
        assert_ne!(stage_id("config"), stage_id("render"));
    }
}
