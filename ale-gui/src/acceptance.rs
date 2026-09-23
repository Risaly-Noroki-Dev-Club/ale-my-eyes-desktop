//! Explicit opt-in package acceptance; never active during normal startup.
use crate::{AppWindow, Ui};
use slint::ComponentHandle;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Start the real desktop UI, optionally using a fresh, isolated acceptance profile.
pub fn setup_app_for_launch(
    app: &AppWindow,
    args: &[String],
) -> Result<Option<slint::Timer>, String> {
    let profile = acceptance_profile(args)?;
    let timer = setup(app, args)?;
    if let Some(profile) = profile {
        crate::desktop_ui::setup_app_with_engine(app, async move {
            let config_path = tokio::task::spawn_blocking(move || prepare_profile(&profile))
                .await
                .map_err(|_| {
                    ale_core::AleError::ConfigError("Acceptance profile worker failed".into())
                })??;
            ale_core::AleEngine::new_with_secret_store(
                &config_path,
                Arc::new(AcceptanceSecretStore::default()),
            )
            .await
        });
    } else {
        crate::desktop_ui::setup_app(app);
    }
    Ok(timer)
}

fn acceptance_profile(args: &[String]) -> Result<Option<PathBuf>, String> {
    let mut indices = args
        .iter()
        .enumerate()
        .filter_map(|(index, arg)| (arg == "--ui-acceptance-profile").then_some(index));
    let Some(index) = indices.next() else {
        return Ok(None);
    };
    if indices.next().is_some() {
        return Err("Specify --ui-acceptance-profile only once".into());
    }
    let acceptance = args.iter().any(|arg| arg == "--ui-acceptance");
    let fault = args.iter().any(|arg| arg == "--ui-fault");
    if acceptance == fault {
        return Err(
            "--ui-acceptance-profile requires exactly one of --ui-acceptance or --ui-fault".into(),
        );
    }
    let profile = PathBuf::from(
        args.get(index + 1)
            .ok_or("Missing acceptance profile directory")?,
    );
    if !profile.is_absolute() {
        return Err("Acceptance profile directory must be absolute".into());
    }
    Ok(Some(profile))
}

fn prepare_profile(profile: &Path) -> ale_core::Result<PathBuf> {
    // Never reuse a profile: even an empty existing directory may belong to the user.
    std::fs::create_dir(profile)?;
    let models = profile.join("models");
    std::fs::create_dir(&models)?;
    let config_path = profile.join("config.json");
    let mut config = ale_core::config::AppConfig::default();
    config.models.models_dir = models.to_string_lossy().into_owned();
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
    ale_core::memory::MemoryStore::load_or_create(profile.join("memory.json"))?;
    Ok(config_path)
}

/// Credentials used by acceptance remain in memory, including later settings saves.
#[derive(Default)]
struct AcceptanceSecretStore(Mutex<[Option<String>; 3]>);

impl AcceptanceSecretStore {
    fn get(&self, index: usize) -> ale_core::Result<Option<String>> {
        self.0.lock().map(|keys| keys[index].clone()).map_err(|_| {
            ale_core::AleError::ConfigError("Acceptance credential store failed".into())
        })
    }

    fn set(&self, index: usize, value: Option<&str>) -> ale_core::Result<()> {
        let mut keys = self.0.lock().map_err(|_| {
            ale_core::AleError::ConfigError("Acceptance credential store failed".into())
        })?;
        keys[index] = value.map(str::to_owned);
        Ok(())
    }
}

impl ale_core::secret_store::SecretStore for AcceptanceSecretStore {
    fn get_api_key(&self) -> ale_core::Result<Option<String>> {
        self.get(0)
    }
    fn set_api_key(&self, key: &str) -> ale_core::Result<()> {
        self.set(0, Some(key))
    }
    fn delete_api_key(&self) -> ale_core::Result<()> {
        self.set(0, None)
    }
    fn get_backup_api_key(&self) -> ale_core::Result<Option<String>> {
        self.get(1)
    }
    fn set_backup_api_key(&self, key: &str) -> ale_core::Result<()> {
        self.set(1, Some(key))
    }
    fn delete_backup_api_key(&self) -> ale_core::Result<()> {
        self.set(1, None)
    }
    fn get_transcription_api_key(&self) -> ale_core::Result<Option<String>> {
        self.get(2)
    }
    fn set_transcription_api_key(&self, key: &str) -> ale_core::Result<()> {
        self.set(2, Some(key))
    }
    fn delete_transcription_api_key(&self) -> ale_core::Result<()> {
        self.set(2, None)
    }
}

pub fn setup(app: &AppWindow, args: &[String]) -> Result<Option<slint::Timer>, String> {
    if let Some(index) = args.iter().position(|arg| arg == "--ui-fault") {
        let fault = args.get(index + 1).ok_or("Missing UI fault mode")?.clone();
        if !matches!(
            fault.as_str(),
            "busy" | "lock" | "panic" | "access-violation"
        ) {
            return Err("Unknown UI fault mode".into());
        }
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_secs(5),
            move || match fault.as_str() {
                "busy" => injected_ui_busy_loop(),
                "lock" => injected_ui_lock_wait(),
                "access-violation" => injected_access_violation(),
                _ => panic!("diagnostic acceptance panic"),
            },
        );
        return Ok(Some(timer));
    }
    let Some(index) = args.iter().position(|arg| arg == "--ui-acceptance") else {
        return Ok(None);
    };
    let output = std::path::PathBuf::from(args.get(index + 1).ok_or("Missing acceptance report")?);
    let seconds = args
        .get(index + 2)
        .ok_or("Missing duration")?
        .parse::<u64>()
        .map_err(|_| "Invalid duration")?
        .max(120);
    let started = Instant::now();
    let state = Rc::new(RefCell::new((0_u64, 0_u64, None::<i32>, 0_u64)));
    let weak = app.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<Ui>();
        let mut sample = state.borrow_mut();
        sample.0 += 1;
        let remaining = ui.get_remaining();
        if sample.2.is_some_and(|old| remaining < old) { sample.1 += 1; }
        sample.2 = Some(remaining);
        if sample.0.is_multiple_of(10) && ui.get_ready() {
            let page = if ui.get_page() == "settings" { "pairing" } else { "settings" };
            ui.invoke_navigate(page.into());
            if ui.get_page() == page { sample.3 += 1; }
        }
        if started.elapsed() >= Duration::from_secs(seconds) {
            let report = serde_json::json!({"duration_seconds":started.elapsed().as_secs(),"ticks":sample.0,"countdown_changes":sample.1,"navigations":sample.3,"ready":ui.get_ready(),"passed":ui.get_ready() && sample.0 >= seconds.saturating_sub(5) && sample.1 >= 30 && sample.3 >= 4});
            let output = output.clone();
            // Report persistence cannot stall the event loop under test.
            std::thread::spawn(move || {
                let _ = std::fs::write(output, report.to_string());
                let _ = slint::quit_event_loop();
            });
        }
    });
    Ok(Some(timer))
}

#[inline(never)]
fn injected_ui_busy_loop() {
    loop {
        std::hint::spin_loop();
    }
}
#[inline(never)]
fn injected_ui_lock_wait() {
    let lock = std::sync::Mutex::new(());
    let _first = lock.lock().unwrap();
    let _second = lock.lock().unwrap();
}

#[inline(never)]
fn injected_access_violation() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::RaiseException(
            0xC0000005,
            1,
            0,
            std::ptr::null(),
        );
    }
    #[cfg(not(windows))]
    panic!("access-violation injection is Windows only");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acceptance_profile_requires_explicit_valid_arguments() {
        let profile = std::env::temp_dir().join("ale-ui-profile-arguments");
        let profile = profile.to_string_lossy().into_owned();
        let args = |values: &[&str]| {
            values
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>()
        };
        assert!(acceptance_profile(&args(&["ale-gui"])).unwrap().is_none());
        assert!(acceptance_profile(&args(&["--ui-acceptance-profile", &profile])).is_err());
        assert!(acceptance_profile(&args(&[
            "--ui-acceptance",
            "out",
            "120",
            "--ui-acceptance-profile"
        ]))
        .is_err());
        assert!(acceptance_profile(&args(&[
            "--ui-acceptance",
            "out",
            "120",
            "--ui-acceptance-profile",
            "relative"
        ]))
        .is_err());
        assert!(acceptance_profile(&args(&[
            "--ui-acceptance",
            "out",
            "120",
            "--ui-fault",
            "lock",
            "--ui-acceptance-profile",
            &profile
        ]))
        .is_err());
        assert!(acceptance_profile(&args(&[
            "--ui-acceptance",
            "out",
            "120",
            "--ui-acceptance-profile",
            &profile,
            "--ui-acceptance-profile",
            &profile
        ]))
        .is_err());
        assert_eq!(
            acceptance_profile(&args(&[
                "--ui-acceptance",
                "out",
                "120",
                "--ui-acceptance-profile",
                &profile
            ]))
            .unwrap(),
            Some(PathBuf::from(&profile))
        );
        assert_eq!(
            acceptance_profile(&args(&[
                "--ui-fault",
                "lock",
                "--ui-acceptance-profile",
                &profile
            ]))
            .unwrap(),
            Some(PathBuf::from(profile))
        );
    }

    #[test]
    fn acceptance_profile_is_fresh_and_refuses_existing_data() {
        let profile = std::env::temp_dir().join(format!("ale-ui-profile-{}", uuid::Uuid::new_v4()));
        let config_path = prepare_profile(&profile).unwrap();
        let original = std::fs::read(&config_path).unwrap();
        let config: ale_core::config::AppConfig = serde_json::from_slice(&original).unwrap();
        assert_eq!(
            PathBuf::from(config.models.models_dir),
            profile.join("models")
        );
        assert!(profile.join("memory.json").is_file());
        assert!(config.cloud_api.api_key.is_empty());
        std::fs::write(profile.join("keep.txt"), b"existing data").unwrap();
        assert!(prepare_profile(&profile).is_err());
        assert_eq!(std::fs::read(&config_path).unwrap(), original);
        assert_eq!(
            std::fs::read(profile.join("keep.txt")).unwrap(),
            b"existing data"
        );
        std::fs::remove_dir_all(profile).unwrap();
    }
}
