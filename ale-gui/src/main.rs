use ale_gui::AppWindow;
use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create Tokio runtime");
    let _runtime_guard = runtime.enter();
    let app = AppWindow::new()?;
    ale_gui::setup_app(&app);
    app.run()
}
