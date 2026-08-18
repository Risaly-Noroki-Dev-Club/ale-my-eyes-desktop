#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use ale_gui::AppWindow;
use slint::ComponentHandle;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create async runtime");
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("--modeld-supervisor-check") {
        let models_dir = args
            .get(2)
            .ok_or("--modeld-supervisor-check requires models and report paths")?;
        let report_path = args
            .get(3)
            .ok_or("--modeld-supervisor-check requires models and report paths")?;
        runtime
            .block_on(ale_gui::run_modeld_supervisor_acceptance(
                models_dir.into(),
                report_path.into(),
            ))
            .map_err(std::io::Error::other)?;
        return Ok(());
    }
    let _runtime_guard = runtime.enter();
    let app = AppWindow::new()?;
    ale_gui::setup_app(&app);
    Ok(app.run()?)
}
