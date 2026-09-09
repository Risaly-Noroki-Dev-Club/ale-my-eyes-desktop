use crate::remote_server::{self, RemoteServerHandle};
use crate::ui_state::Control;
use crate::{AppWindow, DeviceRow, LogRow, ModelRow, Ui};
use ale_core::config::{AppConfig, ConfigValidator};
use ale_core::remote::{ConfirmExecution, DecisionResponse, RemoteMessage};
use ale_core::{AleEngine, AleEngineFactory};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Default)]
struct State {
    engine: Option<Arc<Mutex<AleEngine>>>,
    server: Option<RemoteServerHandle>,
    saved: AppConfig,
    selected: String,
    destination: String,
    test_cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

fn spawn(future: impl std::future::Future<Output = ()> + 'static) {
    if let Err(error) = slint::spawn_local(future) {
        tracing::warn!(%error, "UI task failed");
    }
}

fn feedback(app: &AppWindow, text: &str, error: bool) {
    app.global::<Ui>().set_feedback(text.into());
    app.global::<Ui>().set_feedback_error(error);
}

fn apply(app: &AppWindow, config: &AppConfig) {
    let ui = app.global::<Ui>();
    ui.set_wire_api(protocol_index(config.cloud_api.wire_api));
    let backup = config.remote_routing.backup.clone().unwrap_or_default();
    ui.set_backup_enabled(config.remote_routing.backup_enabled);
    ui.set_backup_authorized(config.remote_routing.backup_pre_authorized);
    ui.set_backup_wire_api(protocol_index(backup.wire_api));
    ui.set_backup_url(backup.api_url.into());
    ui.set_backup_key(backup.api_key.into());
    ui.set_backup_model(backup.model.into());
    ui.set_backup_timeout(backup.timeout.to_string().into());
    ui.set_asr_enabled(config.transcription.enabled);
    ui.set_asr_url(config.transcription.endpoint.api_url.clone().into());
    ui.set_asr_key(config.transcription.endpoint.api_key.clone().into());
    ui.set_asr_model(config.transcription.endpoint.model.clone().into());
    ui.set_asr_timeout(config.transcription.endpoint.timeout.to_string().into());
    ui.set_provider(config.cloud_api.provider.clone().into());
    ui.set_api_key(config.cloud_api.api_key.clone().into());
    ui.set_api_url(config.cloud_api.api_url.clone().into());
    ui.set_model(config.cloud_api.model.clone().into());
    ui.set_timeout(config.cloud_api.timeout.to_string().into());
    ui.set_max_tokens(config.cloud_api.max_tokens.to_string().into());
    ui.set_auto_speak(config.ui.auto_speak);
    ui.set_english(config.ui.language == "en");
    ui.set_high_contrast(config.ui.high_contrast);
    ui.set_theme(config.ui.theme.clone().into());
    ui.set_dirty(false);
    ui.set_reveal_key(false);
}

fn positive(value: &str, label: &str) -> Result<u32, String> {
    value
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("{label}: must be a positive integer / 必须为正整数"))
}

fn draft(app: &AppWindow, base: &AppConfig) -> Result<AppConfig, String> {
    let ui = app.global::<Ui>();
    let mut config = base.clone();
    config.cloud_api.wire_api = protocol(ui.get_wire_api());
    config.remote_routing.backup_enabled = ui.get_backup_enabled();
    config.remote_routing.backup_pre_authorized = ui.get_backup_authorized();
    if ui.get_backup_enabled() {
        if !ui.get_backup_authorized() {
            return Err(
                "Allow automatic failover before enabling backup / 请授权备用端点切换".into(),
            );
        }
        let backup = ale_core::config::CloudApiConfig {
            provider: "custom".into(),
            wire_api: protocol(ui.get_backup_wire_api()),
            api_url: ui.get_backup_url().trim().trim_end_matches('/').into(),
            api_key: ui.get_backup_key().to_string(),
            model: ui.get_backup_model().trim().into(),
            timeout: positive(&ui.get_backup_timeout(), "Backup timeout")?,
            max_tokens: base.cloud_api.max_tokens,
        };
        ConfigValidator::validate_cloud_api(&backup).map_err(|e| e.to_string())?;
        config.remote_routing.backup = Some(backup);
    }
    config.transcription.enabled = ui.get_asr_enabled();
    if ui.get_asr_enabled() {
        config.transcription.endpoint = ale_core::config::CloudApiConfig {
            provider: "openai".into(),
            wire_api: Default::default(),
            api_url: ui.get_asr_url().trim().trim_end_matches('/').into(),
            api_key: ui.get_asr_key().to_string(),
            model: ui.get_asr_model().trim().into(),
            timeout: positive(&ui.get_asr_timeout(), "Transcription timeout")?,
            ..Default::default()
        };
        ConfigValidator::validate_cloud_api(&config.transcription.endpoint)
            .map_err(|e| e.to_string())?;
    }
    config.cloud_api.provider = ui.get_provider().trim().to_string();
    config.cloud_api.api_key = ui.get_api_key().to_string();
    config.cloud_api.api_url = ui.get_api_url().trim().trim_end_matches('/').to_string();
    config.cloud_api.model = ui.get_model().trim().to_string();
    config.cloud_api.timeout = positive(&ui.get_timeout(), "Timeout / 超时")?;
    if config.cloud_api.timeout > 80 {
        return Err("Timeout must be 1-80 seconds / 超时范围为 1-80 秒".into());
    }
    config.cloud_api.max_tokens =
        positive(&ui.get_max_tokens(), "Max tokens / Token 上限")? as usize;
    ConfigValidator::validate_cloud_api_transport(&config.cloud_api.api_url)
        .map_err(|e| e.to_string())?;
    if config.cloud_api.provider.is_empty() || config.cloud_api.model.is_empty() {
        return Err("Provider and model are required / 请填写服务商和模型".into());
    }
    config.ui.auto_speak = ui.get_auto_speak();
    config.ui.high_contrast = ui.get_high_contrast();
    config.ui.language = if ui.get_english() { "en" } else { "zh-CN" }.into();
    Ok(config)
}

fn page(app: &AppWindow, state: &State, target: &str) {
    let ui = app.global::<Ui>();
    if target == "main" && !ui.get_connected() {
        return;
    }
    ui.set_reveal_key(false);
    // Protect both settings and pairing credentials, including the server's capture service.
    if let Some(server) = &state.server {
        server.platform.set_sensitive_ui_visible(true);
    }
    ui.set_page(target.into());
    if target == "main" {
        let weak = app.as_weak();
        let platform = state.server.as_ref().map(|s| s.platform.clone());
        slint::Timer::single_shot(Duration::from_millis(250), move || {
            if weak
                .upgrade()
                .is_some_and(|app| app.global::<Ui>().get_page() == "main")
            {
                if let Some(platform) = platform {
                    platform.set_sensitive_ui_visible(false);
                }
            }
        });
    }
}

fn qr(app: &AppWindow, server: &RemoteServerHandle) {
    let credentials = server.credentials.lock().unwrap();
    let ui = app.global::<Ui>();
    ui.set_pairing_code(credentials.info.code.clone().into());
    ui.set_pairing_uri(credentials.info.uri().into());
    if let Ok(image) = remote_server::render_qr_image(&credentials.info.uri(), false) {
        let mut buffer =
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(image.width(), image.height());
        buffer.make_mut_bytes().copy_from_slice(image.as_raw());
        ui.set_qr(slint::Image::from_rgba8(buffer));
    }
}

pub fn setup_app(app: &AppWindow) {
    let state = Rc::new(RefCell::new(State::default()));
    let ui = app.global::<Ui>();
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.set_build_date(
        option_env!("ALE_BUILD_DATE")
            .unwrap_or("Unknown / 未知")
            .into(),
    );
    ui.set_pairing_status("正在启动 / Starting…".into());
    {
        let weak = app.as_weak();
        let state = state.clone();
        spawn(async move {
            match AleEngineFactory::create_default().await {
                Ok(engine) => {
                    let config = engine.config().clone();
                    let engine = Arc::new(Mutex::new(engine));
                    if let Some(app) = weak.upgrade() {
                        apply(&app, &config);
                        state.borrow_mut().saved = config;
                        state.borrow_mut().engine = Some(engine.clone());
                        app.global::<Ui>().set_ready(true);
                    } else {
                        return;
                    }
                    match remote_server::start(engine).await {
                        Ok(server) => {
                            server.platform.set_sensitive_ui_visible(true);
                            if let Some(app) = weak.upgrade() {
                                qr(&app, &server);
                            }
                            state.borrow_mut().server = Some(server);
                        }
                        Err(error) => {
                            if let Some(app) = weak.upgrade() {
                                feedback(&app, &error, true);
                            }
                        }
                    }
                }
                Err(error) => {
                    if let Some(app) = weak.upgrade() {
                        feedback(&app, &error.to_string(), true);
                    }
                }
            }
        });
    }
    {
        let weak = app.as_weak();
        let state = state.clone();
        ui.on_navigate(move |target| {
            let Some(app) = weak.upgrade() else { return };
            let ui = app.global::<Ui>();
            if ui.get_busy() {
                return;
            }
            if target == "main" && !ui.get_connected() {
                return;
            }
            if ui.get_page() == "settings" && target != "settings" && ui.get_dirty() {
                state.borrow_mut().destination = target.to_string();
                ui.set_leave_prompt(true);
                return;
            }
            if target == "settings" && ui.get_page() != "settings" {
                apply(&app, &state.borrow().saved);
                feedback(&app, "", false);
            }
            page(&app, &state.borrow(), &target);
        });
    }
    {
        let weak = app.as_weak();
        ui.on_edited(move || {
            if let Some(app) = weak.upgrade() {
                app.global::<Ui>().set_dirty(true);
                app.global::<Ui>().set_test_report("".into());
                feedback(&app, "", false);
            }
        });
    }
    {
        let weak = app.as_weak();
        ui.on_reveal(move || {
            let Some(app) = weak.upgrade() else { return };
            let ui = app.global::<Ui>();
            ui.set_reveal_key(!ui.get_reveal_key());
            let weak = app.as_weak();
            slint::Timer::single_shot(Duration::from_secs(10), move || {
                if let Some(app) = weak.upgrade() {
                    app.global::<Ui>().set_reveal_key(false);
                }
            });
        });
    }
    {
        let weak = app.as_weak();
        ui.on_provider_selected(move |provider| {
            let Some(app) = weak.upgrade() else { return };
            let ui = app.global::<Ui>();
            if ui.get_provider() == provider {
                return;
            }
            ui.set_test_report("".into());
            ui.set_wire_api(match provider.as_str() {
                "anthropic" => 2,
                "google" => 3,
                _ => 0,
            });
            ui.set_provider(provider.clone());
            // Never send an existing provider's credential to a newly selected endpoint.
            ui.set_api_key("".into());
            let (url, model) = match provider.as_str() {
                "anthropic" => ("https://api.anthropic.com/v1", "claude-sonnet-4-20250514"),
                "google" => (
                    "https://generativelanguage.googleapis.com/v1beta",
                    "gemini-2.5-flash",
                ),
                "openai" => ("https://api.openai.com/v1", "gpt-4o"),
                _ => ("https://", ""),
            };
            ui.set_api_url(url.into());
            ui.set_model(model.into());
            ui.set_dirty(true);
        });
    }
    {
        let weak = app.as_weak();
        let state = state.clone();
        ui.on_refresh(move || {
            let Some(app) = weak.upgrade() else { return };
            if app.global::<Ui>().get_busy() {
                return;
            }
            if let Some(server) = &state.borrow().server {
                server.refresh_pairing();
                qr(&app, server);
                return;
            }
            let engine = state.borrow().engine.clone();
            if let Some(engine) = engine {
                let state = state.clone();
                let weak = app.as_weak();
                app.global::<Ui>().set_busy(true);
                spawn(async move {
                    let result = remote_server::start(engine).await;
                    if let Some(app) = weak.upgrade() {
                        match result {
                            Ok(server) => {
                                server.platform.set_sensitive_ui_visible(true);
                                qr(&app, &server);
                                state.borrow_mut().server = Some(server);
                                feedback(&app, "", false);
                            }
                            Err(error) => feedback(&app, &error, true),
                        }
                        app.global::<Ui>().set_busy(false);
                    }
                });
            }
        });
    }
    {
        let state = state.clone();
        ui.on_select_device(move |index| {
            let selected = state.borrow().server.as_ref().and_then(|server| {
                server
                    .hub
                    .0
                    .lock()
                    .unwrap()
                    .sessions
                    .keys()
                    .nth(index.max(0) as usize)
                    .cloned()
            });
            if let Some(selected) = selected {
                state.borrow_mut().selected = selected;
            }
        });
    }
    for action in ["pause", "disconnect", "decide"] {
        let state = state.clone();
        let weak = app.as_weak();
        let callback = move |approved: bool| {
            let Some(app) = weak.upgrade() else { return };
            let state = state.borrow();
            let Some(server) = &state.server else { return };
            let mut hub = server.hub.0.lock().unwrap();
            let Some(session) = hub.sessions.get_mut(&state.selected) else {
                return;
            };
            let command = match action {
                "pause" => Control::Pause(!session.paused),
                "disconnect" => Control::Disconnect,
                _ => {
                    let Some(decision) = session.decision.as_ref() else {
                        return;
                    };
                    if session.paused || decision.expires <= Instant::now() {
                        return;
                    }
                    Control::Reply(if let Some(id) = &decision.id {
                        RemoteMessage::DecisionResponse(DecisionResponse {
                            request_id: decision.request.clone(),
                            decision_id: id.clone(),
                            approved,
                        })
                    } else {
                        RemoteMessage::ConfirmExecution(ConfirmExecution {
                            request_id: decision.request.clone(),
                            approved,
                        })
                    })
                }
            };
            match session.controls.try_send(command) {
                Ok(()) => {
                    session.decision = None;
                    app.global::<Ui>().set_can_confirm(false);
                }
                Err(_) => feedback(
                    &app,
                    "Session is busy or disconnected / 会话忙碌或已断开",
                    true,
                ),
            }
        };
        match action {
            "pause" => ui.on_pause(move || callback(false)),
            "disconnect" => ui.on_disconnect(move || callback(false)),
            _ => ui.on_decide(callback),
        }
    }
    {
        let weak = app.as_weak();
        let state = state.clone();
        ui.on_test(move || {
            let Some(app) = weak.upgrade() else { return };
            let ui = app.global::<Ui>();
            if ui.get_busy() {
                return;
            }
            let config = match draft(&app, &state.borrow().saved) {
                Ok(config) => config,
                Err(error) => {
                    feedback(&app, &error, true);
                    return;
                }
            };
            let targets = probe_targets(&config, ui.get_english());
            if targets.is_empty() {
                feedback(
                    &app,
                    "Configure a model endpoint first / 请先配置模型端点",
                    true,
                );
                return;
            }
            for (_, config, _) in &targets {
                if let Err(error) = ConfigValidator::validate_cloud_api(config) {
                    feedback(&app, &error.to_string(), true);
                    return;
                }
            }
            ui.set_test_targets(
                targets
                    .iter()
                    .map(|(name, config, asr)| {
                        format!(
                            "{name}: {}\n{}\n{} {}",
                            config.model,
                            config.api_url,
                            if *asr { 1 } else { 4 },
                            if ui.get_english() {
                                "requests"
                            } else {
                                "次请求"
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into(),
            );
            ui.set_test_prompt(true);
        });
    }
    {
        let weak = app.as_weak();
        let state = state.clone();
        ui.on_cancel_test(move || {
            if let Some(cancel) = state.borrow_mut().test_cancel.take() {
                let _ = cancel.send(());
            }
            if let Some(app) = weak.upgrade() {
                app.global::<Ui>().set_testing(false);
            }
        });
    }
    {
        let weak = app.as_weak();
        let state = state.clone();
        ui.on_run_test(move || {
            let Some(app)=weak.upgrade() else {return};
            let ui=app.global::<Ui>();
            if ui.get_busy() || !ui.get_test_prompt() {return;}
            ui.set_test_prompt(false);
            let config=match draft(&app,&state.borrow().saved) {Ok(c)=>c,Err(e)=>{feedback(&app,&e,true);return;}};
            let en=ui.get_english(); let targets=probe_targets(&config,en);
            let (cancel, cancelled)=tokio::sync::oneshot::channel();
            state.borrow_mut().test_cancel=Some(cancel);
            ui.set_busy(true); ui.set_testing(true); ui.set_test_report("".into());
            let weak=app.as_weak(); let state=state.clone();
            spawn(async move {
                let run=async {
                    for (name,config,asr) in targets {
                        let probes=if asr {vec![ale_core::model_probe::Probe::Transcription]} else {ale_core::model_probe::Probe::PLANNING.to_vec()};
                        for probe in probes {
                            let Some(app)=weak.upgrade() else {return};
                            let ui=app.global::<Ui>();
                            let previous=ui.get_test_report().to_string();
                            ui.set_test_report(format!("{previous}{name} / {}: {}\n",probe.label(en),if en {"Testing..."} else {"测试中…"}).into());
                            drop(app);
                            let started=Instant::now();
                            let result=ale_core::model_probe::run(&AleEngine::cloud_config_from_app(&config),probe).await;
                            let Some(app)=weak.upgrade() else {return};
                            let status=match result {
                                Ok(())=>if en {"Passed".into()} else {"通过".into()},
                                Err(ale_core::AleError::ModelCall(error)) if error.kind==ale_core::model_api::ErrorKind::Unsupported => if en {"Unsupported".into()} else {"不支持".into()},
                                Err(error)=>format!("{}: {error}",if en {"Failed"} else {"失败"}),
                            };
                            app.global::<Ui>().set_test_report(format!("{previous}{name} / {}: {status} ({} ms)\n",probe.label(en),started.elapsed().as_millis()).into());
                        }
                    }
                };
                tokio::select! {
                    _=run=>{},
                    _=cancelled=>{
                        if let Some(app)=weak.upgrade() {
                            let ui=app.global::<Ui>();
                            ui.set_test_report(format!("{}{}\n",ui.get_test_report(),if en {"Cancelled"} else {"已取消"}).into());
                        }
                    }
                }
                state.borrow_mut().test_cancel=None;
                if let Some(app)=weak.upgrade() { app.global::<Ui>().set_busy(false); app.global::<Ui>().set_testing(false); }
            });
        });
    }

    {
        let state = state.clone();
        let weak = app.as_weak();
        ui.on_save(move || {
            if let Some(app) = weak.upgrade() {
                save(&app, state.clone());
            }
        });
    }
    {
        let state = state.clone();
        let weak = app.as_weak();
        ui.on_leave(move |choice| {
            let Some(app) = weak.upgrade() else { return };
            app.global::<Ui>().set_leave_prompt(false);
            match choice {
                1 => save(&app, state.clone()),
                2 => {
                    apply(&app, &state.borrow().saved);
                    let target = std::mem::take(&mut state.borrow_mut().destination);
                    page(&app, &state.borrow(), &target);
                }
                _ => {
                    state.borrow_mut().destination.clear();
                }
            }
        });
    }
    {
        let state = state.clone();
        let weak = app.as_weak();
        spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let Some(app) = weak.upgrade() else { break };
                render_state(&app, &mut state.borrow_mut());
            }
        });
    }
    {
        let weak = app.as_weak();
        spawn(async move {
            loop {
                let client = state
                    .borrow()
                    .server
                    .as_ref()
                    .and_then(|s| s.modeld.clone());
                let health = if let Some(client) = client {
                    tokio::time::timeout(Duration::from_secs(3), client.health())
                        .await
                        .ok()
                        .and_then(Result::ok)
                } else {
                    None
                };
                let Some(app) = weak.upgrade() else { break };
                let en = app.global::<Ui>().get_english();
                let rows = ["SenseVoiceSmall", "Qwen2.5-VL", "ShowUI"]
                    .iter()
                    .map(|name| {
                        use ale_core::model_scheduler::ModelCapability;
                        let capability = match *name {
                            "SenseVoiceSmall" => ModelCapability::SpeechRecognition,
                            "Qwen2.5-VL" => ModelCapability::LocalPlanning,
                            _ => ModelCapability::ElementGrounding,
                        };
                        let state = match &health {
                            None => {
                                if en {
                                    "Unavailable"
                                } else {
                                    "不可用"
                                }
                            }
                            Some(h)
                                if *name == "SenseVoiceSmall"
                                    && matches!(
                                        h.sensevoice_state,
                                        Some(ale_core::model_scheduler::LocalModelState::Busy)
                                    ) =>
                            {
                                if en {
                                    "Busy"
                                } else {
                                    "忙碌"
                                }
                            }
                            Some(h)
                                if *name == "SenseVoiceSmall"
                                    && matches!(
                                        h.sensevoice_state,
                                        Some(ale_core::model_scheduler::LocalModelState::Ready)
                                    ) =>
                            {
                                if en {
                                    "Loaded"
                                } else {
                                    "已加载"
                                }
                            }
                            Some(h)
                                if h.hot_worker
                                    .as_ref()
                                    .is_some_and(|w| w.model_id.contains(name)) =>
                            {
                                if h.hot_worker.as_ref().unwrap().active {
                                    if en {
                                        "Busy"
                                    } else {
                                        "忙碌"
                                    }
                                } else if en {
                                    "Loaded"
                                } else {
                                    "已加载"
                                }
                            }
                            Some(h) if h.available_capabilities.contains(&capability) => {
                                if en {
                                    "Available"
                                } else {
                                    "可用"
                                }
                            }
                            _ => {
                                if en {
                                    "Unavailable"
                                } else {
                                    "不可用"
                                }
                            }
                        };
                        ModelRow {
                            name: (*name).into(),
                            state: state.into(),
                        }
                    })
                    .collect::<Vec<_>>();
                app.global::<Ui>()
                    .set_models(ModelRc::new(VecModel::from(rows)));
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    }
}

fn save(app: &AppWindow, state: Rc<RefCell<State>>) {
    let ui = app.global::<Ui>();
    if ui.get_busy() {
        return;
    }
    let config = match draft(app, &state.borrow().saved) {
        Ok(c) => c,
        Err(e) => {
            feedback(app, &e, true);
            return;
        }
    };
    let Some(engine) = state.borrow().engine.clone() else {
        return;
    };
    let previous = state.borrow().saved.clone();
    let client = state
        .borrow()
        .server
        .as_ref()
        .and_then(|s| s.modeld.clone());
    ui.set_busy(true);
    let weak = app.as_weak();
    spawn(async move {
        let result = async {
            engine
                .lock()
                .await
                .update_config(config.clone())
                .map_err(|e| e.to_string())?;
            if let Some(client) = client {
                if let Err(error) = client.update_config(config.clone()).await {
                    engine.lock().await.update_config(previous).map_err(|_| {
                        "Model sync failed and settings rollback failed; re-enter settings"
                            .to_string()
                    })?;
                    return Err(error);
                }
            }
            Ok::<_, String>(())
        }
        .await;
        if let Some(app) = weak.upgrade() {
            match result {
                Ok(()) => {
                    state.borrow_mut().saved = config.clone();
                    apply(&app, &config);
                    feedback(
                        &app,
                        if app.global::<Ui>().get_english() {
                            "Settings saved"
                        } else {
                            "设置已保存"
                        },
                        false,
                    );
                    let target = std::mem::take(&mut state.borrow_mut().destination);
                    if !target.is_empty() {
                        page(&app, &state.borrow(), &target);
                    }
                }
                Err(error) => feedback(&app, &error, true),
            }
            app.global::<Ui>().set_busy(false);
        }
    });
}

fn render_state(app: &AppWindow, state: &mut State) {
    let ui = app.global::<Ui>();
    let en = ui.get_english();
    let Some(server) = &state.server else {
        ui.set_pairing_status(
            if en {
                "Service unavailable"
            } else {
                "服务未就绪"
            }
            .into(),
        );
        return;
    };
    let remaining = server
        .credentials
        .lock()
        .unwrap()
        .expires
        .saturating_duration_since(Instant::now())
        .as_secs() as i32;
    ui.set_remaining(remaining);
    let mut hub = server.hub.0.lock().unwrap();
    hub.mute_speech = !state.saved.ui.auto_speak;
    if !hub.sessions.contains_key(&state.selected) {
        state.selected = hub.sessions.keys().next().cloned().unwrap_or_default();
    }
    let connected = !hub.sessions.is_empty();
    ui.set_connected(connected);
    ui.set_pairing_status(
        if connected {
            if en {
                "Connected"
            } else {
                "已连接"
            }
        } else if remaining == 0 {
            if en {
                "Expired - refresh the code"
            } else {
                "配对码已过期，请刷新"
            }
        } else if en {
            "Waiting for phone"
        } else {
            "等待手机连接"
        }
        .into(),
    );
    let devices = hub
        .sessions
        .iter()
        .map(|(id, s)| DeviceRow {
            id: id.clone().into(),
            name: s.name.clone().into(),
        })
        .collect::<Vec<_>>();
    if ui.get_devices().iter().collect::<Vec<_>>() != devices {
        ui.set_device_names(ModelRc::new(VecModel::from(
            devices
                .iter()
                .map(|device| device.name.clone())
                .collect::<Vec<_>>(),
        )));
        ui.set_devices(ModelRc::new(VecModel::from(devices)));
    }
    ui.set_selected_device(
        hub.sessions
            .keys()
            .position(|id| id == &state.selected)
            .unwrap_or_default() as i32,
    );
    if let Some(session) = hub.sessions.get_mut(&state.selected) {
        ui.set_device_name(session.name.clone().into());
        ui.set_device_address(session.address.clone().into());
        ui.set_paused(session.paused);
        ui.set_task(crate::ui_text::event(&session.task, en).into());
        ui.set_output(session.output.clone().into());
        ui.set_latency(
            session
                .latency
                .filter(|(_, time)| time.elapsed() < Duration::from_secs(15))
                .map(|(ms, _)| format!("{ms} ms"))
                .unwrap_or_else(|| if en { "No data" } else { "暂无数据" }.into())
                .into(),
        );
        if session
            .decision
            .as_ref()
            .is_some_and(|d| d.expires <= Instant::now())
        {
            session.decision = None;
        }
        ui.set_decision(
            session
                .decision
                .as_ref()
                .map(|d| d.text.clone())
                .unwrap_or_default()
                .into(),
        );
        ui.set_decision_kind(
            session
                .decision
                .as_ref()
                .map(|d| crate::ui_text::event(&d.kind, en))
                .unwrap_or_default()
                .into(),
        );
        ui.set_can_confirm(session.decision.is_some() && !session.paused);
    }
    let logs = hub
        .logs
        .iter()
        .map(|log| LogRow {
            time: format!(
                "{:02}:{:02}:{:02} UTC",
                log.time / 3600 % 24,
                log.time / 60 % 60,
                log.time % 60
            )
            .into(),
            message: crate::ui_text::event(&log.event, en).into(),
            detail: format!(
                "{} · {}",
                log.session.chars().take(8).collect::<String>(),
                log.event
            )
            .into(),
        })
        .collect::<Vec<_>>();
    if ui.get_logs().iter().collect::<Vec<_>>() != logs {
        ui.set_logs(ModelRc::new(VecModel::from(logs)));
    }
    drop(hub);
    if !connected && ui.get_page() == "main" {
        page(app, state, "pairing");
    }
}

fn protocol(index: i32) -> ale_core::model_api::WireApi {
    ale_core::model_api::WireApi::ALL
        .get(index as usize)
        .copied()
        .unwrap_or_default()
}
fn protocol_index(protocol: ale_core::model_api::WireApi) -> i32 {
    ale_core::model_api::WireApi::ALL
        .iter()
        .position(|p| *p == protocol)
        .unwrap_or(0) as i32
}
fn probe_targets(
    config: &AppConfig,
    en: bool,
) -> Vec<(String, ale_core::config::CloudApiConfig, bool)> {
    let mut targets = Vec::new();
    if !config.cloud_api.api_key.is_empty() {
        targets.push((
            if en { "Primary" } else { "主模型" }.into(),
            config.cloud_api.clone(),
            false,
        ));
    }
    if config.remote_routing.backup_enabled {
        if let Some(backup) = &config.remote_routing.backup {
            targets.push((
                if en { "Backup" } else { "备用模型" }.into(),
                backup.clone(),
                false,
            ));
        }
    }
    if config.transcription.enabled {
        targets.push((
            if en { "Transcription" } else { "语音转写" }.into(),
            config.transcription.endpoint.clone(),
            true,
        ));
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use i_slint_backend_testing::ElementHandle;
    use std::cell::Cell;

    #[test]
    fn visible_assistant_controls_reach_root_callbacks() {
        i_slint_backend_testing::init_no_event_loop();
        let app = AppWindow::new().unwrap();
        let ui = app.global::<Ui>();
        let count = Rc::new(Cell::new(0));
        let callback = count.clone();
        ui.on_navigate(move |_| callback.set(callback.get() + 1));
        ElementHandle::find_by_accessible_label(&app, "工作")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(count.get(), 0, "unpaired work navigation is disabled");
        ui.set_connected(true);
        ElementHandle::find_by_accessible_label(&app, "工作")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(count.get(), 1);
        ui.set_page("main".into());
        ui.set_decision("完整的确认内容".into());
        let confirmations = Rc::new(Cell::new(0));
        let callback = confirmations.clone();
        ui.on_decide(move |_| callback.set(callback.get() + 1));
        ElementHandle::find_by_accessible_label(&app, "确认执行")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(confirmations.get(), 0);
        ui.set_can_confirm(true);
        ElementHandle::find_by_accessible_label(&app, "确认执行")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(confirmations.get(), 1);
        ui.set_paused(true);
        ElementHandle::find_by_accessible_label(&app, "确认执行")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(
            confirmations.get(),
            1,
            "pause disables confirmation even through accessibility"
        );
    }

    #[test]
    fn settings_draft_and_work_gate_use_real_state() {
        i_slint_backend_testing::init_no_event_loop();
        let app = AppWindow::new().unwrap();
        let base = AppConfig::default();
        apply(&app, &base);
        let ui = app.global::<Ui>();
        ui.set_api_url("https://example.test/v1/".into());
        ui.set_model("custom-model".into());
        ui.set_api_key("test-only".into());
        ui.set_english(true);
        ui.set_high_contrast(true);
        let draft = draft(&app, &base).unwrap();
        assert_eq!(draft.cloud_api.api_url, "https://example.test/v1");
        assert_eq!(draft.cloud_api.model, "custom-model");
        assert_eq!(draft.ui.language, "en");
        assert!(draft.ui.high_contrast);
        assert_eq!(base.cloud_api.api_url, "https://api.openai.com/v1");
        assert!(!serde_json::to_string(&draft).unwrap().contains("test-only"));
        page(&app, &State::default(), "main");
        assert_eq!(ui.get_page(), "pairing");
        apply(&app, &base);
        assert!(!ui.get_english());
        assert!(!ui.get_high_contrast());
    }
    #[test]
    fn numeric_input_rejects_silent_fallback() {
        for value in ["", "-1", "0", "1.5", "4294967296", "abc"] {
            assert!(positive(value, "value").is_err());
        }
        assert_eq!(positive(" 30 ", "value").unwrap(), 30);
    }

    #[test]
    fn draft_keeps_protocol_backup_and_transcription_independent() {
        i_slint_backend_testing::init_no_event_loop();
        let app = AppWindow::new().unwrap();
        let base = AppConfig::default();
        apply(&app, &base);
        let ui = app.global::<Ui>();
        ui.set_wire_api(1);
        ui.set_backup_enabled(true);
        ui.set_backup_wire_api(2);
        ui.set_backup_key("backup-test".into());
        assert!(
            draft(&app, &base).is_err(),
            "backup consent must be explicit"
        );
        ui.set_backup_authorized(true);
        ui.set_asr_enabled(true);
        ui.set_asr_key("asr-test".into());
        ui.set_asr_url("https://transcriber.test/v1".into());
        ui.set_asr_model("custom-asr".into());
        let config = draft(&app, &base).unwrap();
        assert_eq!(
            config.cloud_api.wire_api,
            ale_core::model_api::WireApi::OpenaiResponses
        );
        assert_eq!(
            config.remote_routing.backup.as_ref().unwrap().wire_api,
            ale_core::model_api::WireApi::AnthropicMessages
        );
        assert_eq!(config.transcription.endpoint.api_key, "asr-test");
        assert_eq!(config.transcription.endpoint.model, "custom-asr");
        assert_eq!(probe_targets(&config, false).len(), 2);
        assert!(base.remote_routing.backup.is_none());
        assert!(!base.transcription.enabled);
    }

    #[test]
    fn capability_consent_modal_blocks_background_commands() {
        use i_slint_backend_testing::ElementHandle;
        use std::cell::Cell;
        i_slint_backend_testing::init_no_event_loop();
        let app = AppWindow::new().unwrap();
        let ui = app.global::<Ui>();
        ui.set_ready(true);
        ui.set_page("settings".into());
        ui.set_test_prompt(true);
        let navigation = Rc::new(Cell::new(0));
        let calls = navigation.clone();
        ui.on_navigate(move |_| calls.set(calls.get() + 1));
        ElementHandle::find_by_accessible_label(&app, "配对")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(navigation.get(), 0);
        let tests = Rc::new(Cell::new(0));
        let calls = tests.clone();
        ui.on_run_test(move || calls.set(calls.get() + 1));
        assert_eq!(tests.get(), 0);
        ElementHandle::find_by_accessible_label(&app, "开始完整测试")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(tests.get(), 1);
    }
}
