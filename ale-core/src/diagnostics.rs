//! Bounded, content-free process diagnostics. Writers never call tracing themselves.
use serde_json::{json, Map, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{
    field::{Field, Visit},
    span::{Attributes, Id, Record},
    Event, Metadata, Subscriber,
};

const QUEUE_LIMIT: usize = 4096;
const RECORD_LIMIT: usize = 2048;
const ROTATE_BYTES: u64 = 4 * 1024 * 1024;
pub const TEXT_BUDGET: u64 = 256 * 1024 * 1024;
pub const DUMP_BUDGET: u64 = 250 * 1024 * 1024;
pub const MAX_DUMP_BYTES: u64 = 50 * 1024 * 1024;
pub const RETENTION: Duration = Duration::from_secs(7 * 86400);

#[derive(Default)]
pub struct Health {
    pub dropped: AtomicU64,
    pub write_failures: AtomicU64,
    pub last_write_unix_ms: AtomicU64,
    pub fallback_active: AtomicBool,
}
pub fn fallback_directory() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("ale-my-eyes/Ale-My-Eyes-Logs")
}
enum WriterTask {
    Record(Vec<u8>),
    Flush(mpsc::SyncSender<bool>),
}

pub fn build_info() -> Value {
    json!({"id":env!("ALE_BUILD_ID"),"target":env!("ALE_BUILD_TARGET"),"profile":env!("ALE_BUILD_PROFILE"),"cloud":cfg!(feature="cloud"),"local_inference":cfg!(feature="local-inference")})
}
pub fn thread_id() -> u64 {
    #[cfg(windows)]
    {
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() as u64 }
    }
    #[cfg(target_os = "macos")]
    {
        let mut tid = 0;
        unsafe {
            libc::pthread_threadid_np(0, &mut tid);
        }
        tid
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        unsafe { libc::syscall(libc::SYS_gettid) as u64 }
    }
    #[cfg(not(any(
        windows,
        target_os = "macos",
        target_os = "linux",
        target_os = "android"
    )))]
    {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        std::thread::current().id().hash(&mut hash);
        hash.finish()
    }
}
#[derive(Clone)]
pub struct Sink {
    sender: mpsc::SyncSender<WriterTask>,
    started: Instant,
    sequence: Arc<AtomicU64>,
    pub health: Arc<Health>,
    pub directory: PathBuf,
    pub session: String,
    component: &'static str,
}
static GLOBAL: OnceLock<Sink> = OnceLock::new();
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn safe_session(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
impl Sink {
    /// No directory creation or file access is performed by the calling thread.
    pub fn start(directory: PathBuf, session: String, component: &'static str) -> io::Result<Self> {
        if !safe_session(&session) || !safe_session(component) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid diagnostic identity",
            ));
        }
        let (sender, receiver) = mpsc::sync_channel::<WriterTask>(QUEUE_LIMIT);
        let health = Arc::new(Health::default());
        let writer_health = health.clone();
        let root = directory.clone();
        let writer_session = session.clone();
        std::thread::Builder::new()
            .name("ale-event-writer".into())
            .spawn(move || {
                let mut root = root;
                let mut file: Option<File> = None;
                let mut bytes = ROTATE_BYTES;
                let mut sequence = 0u64;
                let mut retry_after = Instant::now();
                while let Ok(task) = receiver.recv() {
                    let record = match task {
                        WriterTask::Record(record) => record,
                        WriterTask::Flush(reply) => {
                            let success = file.as_mut().is_some_and(|file| file.flush().is_ok());
                            let _ = reply.try_send(success);
                            continue;
                        }
                    };
                    if Instant::now() < retry_after {
                        writer_health.dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if bytes >= ROTATE_BYTES || file.is_none() {
                        // Release the previous file's advisory lock before retention runs.
                        file = None;
                        let result = fs::create_dir_all(&root).and_then(|_| {
                            sequence += 1;
                            let path = root.join(format!(
                                "ale-events-{writer_session}-{component}-{}-{sequence}.jsonl",
                                std::process::id()
                            ));
                            let opened = OpenOptions::new()
                                .create_new(true)
                                .write(true)
                                .open(&path)?;
                            if let Err(error) = opened.try_lock().map_err(io::Error::other) {
                                let _ = fs::remove_file(&path);
                                return Err(error);
                            }
                            if let Err(error) = register_file(&path) {
                                let _ = fs::remove_file(&path);
                                return Err(error);
                            }
                            Ok(opened)
                        });
                        match result {
                            Ok(opened) => {
                                file = Some(opened);
                                bytes = 0;
                                let _ = prune(&root);
                            }
                            Err(_) => {
                                writer_health.write_failures.fetch_add(1, Ordering::Relaxed);
                                writer_health.dropped.fetch_add(1, Ordering::Relaxed);
                                // A failure never re-enters the tracing subscriber.
                                root = fallback_directory();
                                writer_health.fallback_active.store(true, Ordering::Relaxed);
                                retry_after = Instant::now() + Duration::from_secs(1);
                                continue;
                            }
                        }
                    }
                    if file.as_mut().unwrap().write_all(&record).is_err() {
                        writer_health.write_failures.fetch_add(1, Ordering::Relaxed);
                        writer_health.dropped.fetch_add(1, Ordering::Relaxed);
                        file = None;
                        root = fallback_directory();
                        writer_health.fallback_active.store(true, Ordering::Relaxed);
                        retry_after = Instant::now() + Duration::from_secs(1);
                    } else {
                        bytes += record.len() as u64;
                        writer_health
                            .last_write_unix_ms
                            .store(unix_ms(), Ordering::Relaxed);
                    }
                }
                if let Some(mut file) = file {
                    let _ = file.flush();
                }
            })?;
        Ok(Self {
            sender,
            started: Instant::now(),
            sequence: Arc::new(AtomicU64::new(1)),
            health,
            directory,
            session,
            component,
        })
    }
    fn submit(&self, mut value: Value) {
        value["schema_version"] = json!(2);
        value["timestamp_unix_ms"] = json!(unix_ms());
        value["session"] = json!(self.session);
        value["component"] = json!(self.component);
        value["pid"] = json!(std::process::id());
        value["tid"] = json!(thread_id());
        value["monotonic_ms"] = json!(self.started.elapsed().as_millis() as u64);
        value["event_sequence"] = json!(self.sequence.fetch_add(1, Ordering::Relaxed));
        value["operation_id"] = value["fields"]["operation_id"]
            .as_u64()
            .map_or(json!(0), |id| json!(id));
        value["build"] = build_info();
        value["version"] = json!(env!("CARGO_PKG_VERSION"));
        let Ok(mut bytes) = serde_json::to_vec(&value) else {
            return;
        };
        if bytes.len() >= RECORD_LIMIT {
            self.health.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        bytes.push(b'\n');
        if self.sender.try_send(WriterTask::Record(bytes)).is_err() {
            self.health.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    /// Call only after leaving the event loop or in a helper process. Never waits indefinitely.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.sender.try_send(WriterTask::Flush(sender)).is_ok()
            && receiver.recv_timeout(timeout).unwrap_or(false)
    }
    /// Names are source literals; dynamic strings (URLs, keys, prompts, paths) are not accepted.
    pub fn record(&self, name: &'static str, fields: &[(&'static str, u64)]) {
        let fields: Map<String, Value> = fields
            .iter()
            .take(16)
            .map(|(key, value)| ((*key).to_string(), json!(value)))
            .collect();
        self.submit(json!({"event": name, "fields": fields}));
    }
}
pub fn current() -> Option<&'static Sink> {
    GLOBAL.get()
}
/// Stable pseudonymous correlation for generated request IDs. Never pass content.
pub fn correlation_id(id: &str) -> u64 {
    id.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    })
}
pub fn record(name: &'static str, fields: &[(&'static str, u64)]) {
    if let Some(sink) = current() {
        sink.record(name, fields);
    }
}
pub fn install(
    directory: PathBuf,
    session: String,
    component: &'static str,
) -> io::Result<&'static Sink> {
    if let Some(sink) = GLOBAL.get() {
        return Ok(sink);
    }
    let sink = Sink::start(directory, session, component)?;
    let _ = GLOBAL.set(sink.clone());
    let _ = tracing::subscriber::set_global_default(DiagnosticSubscriber(sink));
    Ok(GLOBAL.get().unwrap())
}
/// Model processes inherit only a destination and a correlation identifier, never credentials.
pub fn install_from_env(component: &'static str) -> io::Result<bool> {
    let (Some(directory), Ok(session)) = (
        std::env::var_os("ALE_DIAGNOSTIC_DIRECTORY"),
        std::env::var("ALE_DIAGNOSTIC_SESSION"),
    ) else {
        return Ok(false);
    };
    install(directory.into(), session, component)?;
    record("process_start", &[]);
    Ok(true)
}

struct DiagnosticSubscriber(Sink);
struct NumericFields(Map<String, Value>);
impl Visit for NumericFields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if self.0.len() < 12 {
            self.0.insert(field.name().into(), json!(value));
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if self.0.len() < 12 {
            self.0.insert(field.name().into(), json!(value));
        }
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        if self.0.len() < 12 {
            self.0.insert(field.name().into(), json!(value));
        }
    }
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
    // Formatted messages/error values may contain secrets. Record their source location instead.
    fn record_str(&mut self, _: &Field, _: &str) {}
}
impl Subscriber for DiagnosticSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::INFO
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Id::from_u64(NEXT.fetch_add(1, Ordering::Relaxed))
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = NumericFields(Map::new());
        event.record(&mut fields);
        let meta = event.metadata();
        self.0.submit(json!({ "event": "trace", "level": meta.level().as_str(),
            "target": meta.target(), "source": meta.file(), "line": meta.line(), "fields": fields.0 }));
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

pub fn owned_diagnostic_file(name: &str) -> bool {
    (name.starts_with("ale-diagnostic-") && name.ends_with(".json"))
        || (name.starts_with("ale-events-") && name.ends_with(".jsonl"))
        || (name.starts_with("ale-hang-") && (name.ends_with(".dmp") || name.ends_with(".json")))
        || (name.starts_with("ale-crash-") && (name.ends_with(".dmp") || name.ends_with(".json")))
}
fn receipt(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".ale-owned-{}",
        path.file_name().unwrap_or_default().to_string_lossy()
    ))
}
/// Register only files just created by this feature. Retention ignores unregistered files.
pub fn register_file(path: &Path) -> io::Result<()> {
    let mut owner = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(receipt(path))?;
    owner.write_all(b"ale-diagnostics-v2")
}
fn registered(path: &Path) -> bool {
    use std::io::Read;
    File::open(receipt(path)).ok().is_some_and(|file| {
        let mut bytes = Vec::new();
        file.take(32).read_to_end(&mut bytes).is_ok() && bytes == b"ale-diagnostics-v2"
    })
}
/// Active event files hold a cross-process lock. Never delete files held by live writers.
fn remove_inactive(path: &Path) -> io::Result<bool> {
    let file = OpenOptions::new().write(true).open(path)?;
    if file.try_lock().is_err() {
        return Ok(false);
    }
    fs::remove_file(path)?;
    let _ = fs::remove_file(receipt(path));
    Ok(true)
}
/// Unknown files and current .partial files are never removed.
pub fn prune(directory: &Path) -> io::Result<()> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !owned_diagnostic_file(&name)
            || !entry.file_type()?.is_file()
            || !registered(&entry.path())
        {
            continue;
        }
        let meta = entry.metadata()?;
        let modified = meta.modified()?;
        if modified.elapsed().unwrap_or_default() > RETENTION {
            let _ = remove_inactive(&entry.path());
        } else {
            entries.push((modified, entry.path(), meta.len(), name.ends_with(".dmp")));
        }
    }
    entries.sort_by_key(|entry| entry.0);
    for dump in [false, true] {
        let budget = if dump { DUMP_BUDGET } else { TEXT_BUDGET };
        let mut size: u64 = entries.iter().filter(|e| e.3 == dump).map(|e| e.2).sum();
        let mut count = entries.iter().filter(|e| e.3 == dump).count();
        for (_, path, bytes, is_dump) in &entries {
            if *is_dump == dump
                && (size > budget || (dump && count > 5))
                && remove_inactive(path).unwrap_or(false)
            {
                size = size.saturating_sub(*bytes);
                count -= 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tracing_does_not_persist_credentials_or_message_bodies() {
        let dir = std::env::temp_dir().join(format!("ale-diag-{}", uuid::Uuid::new_v4()));
        let sink = Sink::start(dir.clone(), "test".into(), "test").unwrap();
        let health = sink.health.clone();
        tracing::subscriber::with_default(DiagnosticSubscriber(sink), || {
            tracing::warn!(api_key = "secret-credential", error = %"private prompt", bytes = 42u64, "secret response");
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while health.last_write_unix_ms.load(Ordering::Relaxed) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let entry = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("ale-events-")
            })
            .unwrap();
        let text = fs::read_to_string(entry.path()).unwrap();
        assert!(text.contains("42"));
        for secret in ["secret-credential", "private prompt", "secret response"] {
            assert!(!text.contains(secret));
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn full_queue_and_oversize_records_are_bounded_and_counted() {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_LIMIT);
        let sink = Sink {
            sender,
            started: Instant::now(),
            sequence: Arc::new(AtomicU64::new(1)),
            health: Arc::new(Health::default()),
            directory: PathBuf::new(),
            session: "test".into(),
            component: "test",
        };
        for _ in 0..QUEUE_LIMIT + 5 {
            sink.record("sample", &[]);
        }
        assert_eq!(sink.health.dropped.load(Ordering::Relaxed), 5);
        sink.submit(json!({"oversized": "x".repeat(RECORD_LIMIT)}));
        assert_eq!(sink.health.dropped.load(Ordering::Relaxed), 6);
        let WriterTask::Record(bytes) = receiver.try_recv().unwrap() else {
            panic!("record expected")
        };
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(bytes.len() <= RECORD_LIMIT);
        assert!(value["tid"].as_u64().unwrap() > 0);
        assert!(value["build"]["target"].is_string());
        assert!(value["monotonic_ms"].is_u64());
    }
    #[test]
    fn retention_preserves_unregistered_and_live_files() {
        let dir = std::env::temp_dir().join(format!("ale-diag-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let unknown = dir.join("ale-hang-unregistered.dmp");
        fs::write(&unknown, [1]).unwrap();
        let active = dir.join("ale-events-test.jsonl");
        let file = File::create(&active).unwrap();
        file.lock().unwrap();
        register_file(&active).unwrap();
        assert!(!remove_inactive(&active).unwrap());
        prune(&dir).unwrap();
        assert!(unknown.exists());
        drop(file);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn retention_keeps_unknown_files_and_limits_dumps() {
        let dir = std::env::temp_dir().join(format!("ale-diag-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("my-data.dmp"), "owned by user").unwrap();
        for i in 0..8 {
            let path = dir.join(format!("ale-hang-{i}.dmp"));
            fs::write(&path, [1]).unwrap();
            register_file(&path).unwrap();
        }
        prune(&dir).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 11);
        assert!(dir.join("my-data.dmp").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
