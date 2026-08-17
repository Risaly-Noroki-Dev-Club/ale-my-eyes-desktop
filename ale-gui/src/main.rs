#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use ale_gui::AppWindow;
use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create async runtime");
    let _runtime_guard = runtime.enter();
    let app = AppWindow::new()?;
    ale_gui::setup_app(&app);
    app.run()
}
