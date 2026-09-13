#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use ale_gui::AppWindow;
use slint::ComponentHandle;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(result) = ale_gui::diagnostics::dispatch_internal_mode() {
        return result.map_err(|error| std::io::Error::other(error).into());
    }
    if let Some(result) = ale_gui::dispatch_platform_worker() {
        return result.map_err(|error| std::io::Error::other(error).into());
    }
    #[cfg(target_os = "windows")]
    if std::env::var_os("SLINT_BACKEND").is_none() {
        std::env::set_var("SLINT_BACKEND", "winit-software");
    }
    let diagnostics = ale_gui::diagnostics::bootstrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create async runtime");
    let heartbeat = diagnostics.clone();
    runtime.spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_millis(250));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            heartbeat.runtime_heartbeat();
            if heartbeat
                .status
                .shutdown
                .load(std::sync::atomic::Ordering::Acquire)
            {
                break;
            }
        }
    });
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("--modeld-supervisor-check") {
        let models_dir = args
            .get(2)
            .ok_or("--modeld-supervisor-check requires models and report paths")?;
        let report_path = args
            .get(3)
            .ok_or("--modeld-supervisor-check requires models and report paths")?;
        let result = runtime
            .block_on(ale_gui::run_modeld_supervisor_acceptance(
                models_dir.into(),
                report_path.into(),
            ))
            .map_err(std::io::Error::other);
        ale_gui::diagnostics::shutdown_requested();
        runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        return result.map_err(Into::into);
    }
    let result: Result<(), Box<dyn std::error::Error>> = runtime.block_on(async {
        let app = {
            let _stage = ale_gui::diagnostics::stage("window_create");
            AppWindow::new()?
        };
        let supported = app
            .window()
            .set_rendering_notifier(|state, _| {
                let phase = match state {
                    slint::RenderingState::RenderingSetup => 1,
                    slint::RenderingState::BeforeRendering => 2,
                    slint::RenderingState::AfterRendering => 3,
                    slint::RenderingState::RenderingTeardown => 4,
                    _ => 0,
                };
                ale_gui::diagnostics::rendering_state(phase);
            })
            .is_ok();
        ale_gui::diagnostics::render_notifier_supported(supported);
        ale_core::diagnostics::record(
            "renderer_selected",
            &[
                (
                    "software",
                    u64::from(std::env::var("SLINT_BACKEND").is_ok_and(|s| s.contains("software"))),
                ),
                ("render_notifier_supported", u64::from(supported)),
            ],
        );
        ale_gui::setup_app(&app);
        let _acceptance = ale_gui::setup_acceptance(&app, &args)?;
        Ok(app.run()?)
    });
    runtime.block_on(ale_gui::finish_settings_writes());
    ale_gui::diagnostics::shutdown_requested();
    drop(diagnostics);
    // A native library may not return from a blocking call. Do not block closing
    // the GUI on Tokio's implicit, unlimited blocking-pool shutdown wait.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}
