use crate::{AppWindow, Ui};
use ale_core::desktop_download::{self, Progress};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

#[derive(Default)]
struct DownloadState {
    active: bool,
    progress: Progress,
    result: Option<String>,
}

pub struct Tools {
    _timer: slint::Timer,
    cancel: Arc<AtomicBool>,
}
impl Drop for Tools {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub fn setup(app: &AppWindow) -> Tools {
    let diagnostics = crate::diagnostics::bootstrap();
    app.global::<Ui>()
        .set_log_directory(diagnostics.directory.to_string_lossy().into_owned().into());
    let diagnostics = Rc::new(Some(diagnostics));
    app.global::<Ui>().on_diagnostic_setting_changed({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                crate::diagnostics::set_auto_dump_enabled(app.global::<Ui>().get_auto_dump());
            }
        }
    });
    let download = Arc::new(Mutex::new(DownloadState::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    let ui = app.global::<Ui>();
    {
        let weak = app.as_weak();
        ui.on_choose_download_directory(move || {
            let weak = weak.clone();
            let _ = slint::spawn_local(async move {
                if let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await {
                    if let Some(app) = weak.upgrade() {
                        if !app.global::<Ui>().get_downloading() {
                            app.global::<Ui>().set_download_directory(
                                folder.path().to_string_lossy().into_owned().into(),
                            );
                        }
                    }
                }
            });
        });
    }
    {
        let diagnostics = diagnostics.clone();
        ui.on_open_logs(move || {
            if let Some(diagnostics) = diagnostics.as_ref() {
                let directory = diagnostics.actual_directory();
                std::thread::spawn(move || {
                    let _ = open::that(directory);
                });
            }
        });
    }
    {
        let cancel = cancel.clone();
        ui.on_cancel_download(move || cancel.store(true, Ordering::Relaxed));
    }
    {
        let weak = app.as_weak();
        let download = download.clone();
        let cancel = cancel.clone();
        ui.on_start_download(move || {
            let Some(app) = weak.upgrade() else { return };
            let ui = app.global::<Ui>();
            if !ui.get_download_consent() { return; }
            let Ok(mut state) = download.try_lock() else { return };
            if state.active { return; }
            let root = std::path::PathBuf::from(ui.get_download_directory().as_str());
            if !root.is_absolute() {
                ui.set_download_status("请选择绝对路径 / Select an absolute directory".into());
                return;
            }
            let index = ui.get_download_model().max(0) as usize;
            *state = DownloadState { active: true, ..Default::default() };
            drop(state);
            cancel.store(false, Ordering::Relaxed);
            ui.set_downloading(true);
            ui.set_download_status("正在连接下载源… / Connecting…".into());
            let state = download.clone();
            let progress_state = state.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                let report = Arc::new(move |progress| {
                    if let Ok(mut state) = progress_state.lock() { state.progress = progress; }
                });
                let result = desktop_download::download(index, root, cancel, report).await;
                if let Ok(mut state) = state.lock() {
                    state.active = false;
                    state.result = Some(match result {
                        Ok(()) if index == 0 && cfg!(all(windows, target_env = "gnu")) => "模型已校验；此构建不支持本地语音识别，请使用 Windows MSVC 完整功能包。 / Verified; this build lacks local ASR. Use the MSVC package.".into(),
                        Ok(()) if index == 0 => "模型已校验。重启后检测运行时；下载完成不代表识别已就绪。 / Verified. Restart to check runtime readiness.".into(),
                        Ok(()) => "原始模型文件下载完成；仍需转换为 GGUF 并准备视觉投影文件和运行时。 / Source files downloaded; GGUF conversion, vision projector and runtime still required.".into(),
                        Err(error) => format!("下载未完成 / Download incomplete: {error}"),
                    });
                }
            });
        });
    }
    let timer = slint::Timer::default();
    let weak = app.as_weak();
    // Keep diagnostics and the timer alive without relying on the async runtime.
    let last_text = Rc::new(RefCell::new(String::new()));
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(200),
        move || {
            let Some(app) = weak.upgrade() else { return };
            let ui = app.global::<Ui>();
            ui.set_auto_dump(crate::diagnostics::auto_dump_enabled());
            if let Some(diagnostics) = diagnostics.as_ref() {
                ui.set_log_directory(
                    diagnostics
                        .actual_directory()
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                );
            }
            if let Some(diagnostics) = diagnostics.as_ref() {
                diagnostics.heartbeat(ui.get_busy(), ui.get_ready(), ui.get_connected());
            }
            if let Ok(mut state) = download.try_lock() {
                ui.set_downloading(state.active);
                if let Some(diagnostics) = diagnostics.as_ref() {
                    diagnostics
                        .status
                        .downloading
                        .store(state.active, Ordering::Relaxed);
                    diagnostics
                        .status
                        .downloaded_bytes
                        .store(state.progress.downloaded, Ordering::Relaxed);
                }
                let text = if let Some(result) = state.result.take() {
                    result
                } else if state.active && !state.progress.file.is_empty() {
                    format!(
                        "{} · {} · {:.1} / {:.1} MiB",
                        match state.progress.phase {
                            "connecting" => "连接中",
                            "extracting" => "解压中",
                            "verifying" => "校验中",
                            "committing" => "安装中",
                            "cancelling" => "取消并清理中",
                            _ => "下载中",
                        },
                        state.progress.file,
                        state.progress.phase_completed as f64 / 1048576.0,
                        state.progress.phase_total as f64 / 1048576.0
                    )
                } else {
                    return;
                };
                if *last_text.borrow() != text {
                    ui.set_download_status(text.clone().into());
                    *last_text.borrow_mut() = text;
                }
            }
        },
    );
    Tools {
        _timer: timer,
        cancel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use i_slint_backend_testing::ElementHandle;
    use std::cell::Cell;

    #[test]
    fn accessible_downloader_entry_requires_consent_and_exposes_cancel() {
        i_slint_backend_testing::init_no_event_loop();
        let app = AppWindow::new().unwrap();
        let ui = app.global::<Ui>();
        ui.set_page("settings".into());
        ui.set_ready(true);
        ElementHandle::find_by_accessible_label(&app, "模型下载器")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert!(ui.get_show_downloader());
        let starts = Rc::new(Cell::new(0));
        let count = starts.clone();
        ui.on_start_download(move || count.set(count.get() + 1));
        ElementHandle::find_by_accessible_label(&app, "开始下载 / 重试")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(starts.get(), 0);
        ui.set_download_consent(true);
        ElementHandle::find_by_accessible_label(&app, "开始下载 / 重试")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(starts.get(), 1);
        ui.set_downloading(true);
        let cancellations = Rc::new(Cell::new(0));
        let count = cancellations.clone();
        ui.on_cancel_download(move || count.set(count.get() + 1));
        ElementHandle::find_by_accessible_label(&app, "取消下载")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(cancellations.get(), 1);
        ui.set_show_downloader(false);
        assert!(
            ElementHandle::find_by_accessible_label(&app, "打开桌面日志文件夹")
                .next()
                .is_some()
        );
    }
}
