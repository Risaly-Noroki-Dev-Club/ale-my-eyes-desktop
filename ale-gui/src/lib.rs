pub mod audio;
mod audit;
pub mod automation;
mod conversation;
mod desktop_ui;
pub mod file_picker;
mod modeld;
mod platform;
mod remote_crypto;
mod remote_server;
pub mod screen_capture;
pub mod tts_player;
mod ui_state;
mod ui_text;

slint::include_modules!();
pub use desktop_ui::setup_app;

pub async fn run_modeld_supervisor_acceptance(
    models_dir: std::path::PathBuf,
    report_path: std::path::PathBuf,
) -> Result<(), String> {
    modeld::run_supervisor_acceptance(models_dir, report_path).await
}
