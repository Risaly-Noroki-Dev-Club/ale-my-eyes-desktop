//! Explicit opt-in package acceptance; never active during normal startup.
use crate::{AppWindow, Ui};
use slint::ComponentHandle;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

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
