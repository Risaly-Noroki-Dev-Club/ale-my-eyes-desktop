mod acceptance;
mod audit;
pub mod automation;
mod conversation;
mod desktop_ui;
pub mod diagnostics;
mod model_download_ui;
mod modeld;
mod platform;
mod remote_crypto;
mod remote_server;
pub mod screen_capture;
mod settings_service;
mod ui_state;
mod ui_text;
pub mod uia_environment;
pub mod verified_authorizer;
pub mod verified_planner;
pub mod verified_task;

slint::include_modules!();
pub use acceptance::setup as setup_acceptance;
pub use acceptance::setup_app_for_launch;
pub use desktop_ui::setup_app;
pub use platform_worker::dispatch as dispatch_platform_worker;
mod platform_worker;

pub async fn run_modeld_supervisor_acceptance(
    models_dir: std::path::PathBuf,
    report_path: std::path::PathBuf,
) -> Result<(), String> {
    modeld::run_supervisor_acceptance(models_dir, report_path).await
}

pub async fn finish_settings_writes() {
    settings_service::drain().await;
}
