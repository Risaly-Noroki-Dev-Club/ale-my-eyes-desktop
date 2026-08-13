pub mod audio;
mod audit;
mod conversation;
pub mod file_picker;
pub mod tts_player;

pub mod screen_capture;

pub mod automation;

mod platform;
mod remote_crypto;

mod remote_server;

use ale_core::actions::ActionPlan;
use ale_core::config::AppConfig;
#[cfg(target_os = "android")]
use ale_core::remote::CommandInput;
use ale_core::vad::{VadConfig, VadState, VoiceActivityDetector};
use ale_core::{AleEngine, AleEngineFactory};
use conversation::handle_question_response;
use platform::PlatformService;
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

#[cfg(target_os = "android")]
use base64::Engine;

slint::include_modules!();

pub struct AppState {
    engine: Option<Arc<Mutex<AleEngine>>>,
    recorder: Option<audio::Recorder>,
    recording_started: Option<Instant>,
    vad_sample_offset: usize,
    auto_speak: bool,
    vad: VoiceActivityDetector,
    vad_active: bool,
    vad_frame_count: u64,
    listening_enabled: bool,
    platform: Option<Box<dyn PlatformService>>,
    pending_plan: Option<ActionPlan>,
    #[cfg(target_os = "android")]
    pending_remote_request_id: Option<String>,
    #[cfg(target_os = "android")]
    remote_client: Option<remote_client::RemoteClient>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            engine: None,
            recorder: None,
            recording_started: None,
            vad_sample_offset: 0,
            auto_speak: true,
            vad: VoiceActivityDetector::with_default_config(),
            vad_active: false,
            vad_frame_count: 0,
            listening_enabled: true,
            platform: None,
            pending_plan: None,
            #[cfg(target_os = "android")]
            pending_remote_request_id: None,
            #[cfg(target_os = "android")]
            remote_client: None,
        }
    }
}

pub fn setup_app(app: &AppWindow) {
    // 在 Android 上启用移动端触控优化
    #[cfg(target_os = "android")]
    app.set_is_mobile(true);

    let state = Arc::new(Mutex::new(AppState::new()));
    let app_weak = app.as_weak();

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        spawn_local_task(async move {
            loop {
                let engine = { state.lock().await.engine.clone() };
                if let Some(engine) = engine {
                    let Some(app) = app_weak.upgrade() else {
                        return;
                    };
                    match remote_server::start(engine).await {
                        Ok(handle) => {
                            app.set_show_settings(false);
                            app.set_remote_connected(true);
                            app.set_remote_status("桌面端已就绪，请使用移动端扫描二维码".into());
                            app.set_remote_pairing_qr(handle.qr_image);
                        }
                        Err(error) => {
                            app.set_remote_status(slint::format!("桌面端服务启动失败: {}", error));
                            app.set_remote_connected(false);
                        }
                    }
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        });
    }

    // Initialize engine + start monitoring
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        spawn_local_task(async move {
            let result = create_engine().await;
            let mut st = state.lock().await;
            let Some(app) = app_weak.upgrade() else {
                return;
            };

            match result {
                Ok((engine, config)) => {
                    apply_config_to_app(&app, &config);
                    let config_path = ale_core::config::ConfigFactory::create_default()
                        .config_path()
                        .to_string_lossy()
                        .to_string();
                    app.set_config_path(config_path.into());

                    st.engine = Some(engine);
                    // 应用弱语音模式 VAD 配置
                    if config.asr.weak_voice_mode {
                        let weak_vad = VadConfig::weak_voice();
                        st.vad = VoiceActivityDetector::new(weak_vad);
                        tracing::info!("Weak voice VAD mode enabled");
                    }
                    app.set_engine_ready(true);
                    app.set_status_text("就绪".into());
                    app.set_status_type("ready".into());

                    // 创建平台服务。Android 目前只作为局域网指令入口骨架。
                    let platform = platform::create_platform();
                    let capabilities = platform.capabilities();
                    app.set_capability_text(
                        format!(
                            "{}{}{}",
                            if capabilities.local_microphone {
                                "麦克风 + "
                            } else {
                                ""
                            },
                            if capabilities.image_capture {
                                "视觉"
                            } else {
                                "无本机视觉"
                            },
                            if capabilities.automation {
                                " + 自动化"
                            } else {
                                ""
                            }
                        )
                        .into(),
                    );
                    st.platform = Some(platform);

                    // Auto-start continuous listening
                    start_continuous_listening(&mut st, &app);
                }
                Err(error) => {
                    app.set_status_text(slint::format!("初始化失败: {}", error));
                    app.set_status_type("error".into());
                }
            }
        });
    }

    // VAD timer — checks for speech end every 100ms
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        let vad_timer = slint::Timer::default();
        vad_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(100),
            move || {
                let state = state.clone();
                let app_weak = app_weak.clone();
                spawn_local_task(async move {
                    let mut st = state.lock().await;
                    if !st.vad_active || st.recorder.is_none() {
                        return;
                    }

                    let mut vad_sample_offset = st.vad_sample_offset;
                    let samples = if let Some(ref recorder) = st.recorder {
                        recorder.samples_since(&mut vad_sample_offset)
                    } else {
                        return;
                    };
                    st.vad_sample_offset = vad_sample_offset;

                    if samples.is_empty() {
                        return;
                    }

                    let pcm = ale_core::vad::pcm16_bytes_to_f32(&samples);
                    let mut speech_ended = false;
                    for chunk in pcm.chunks(st.vad.config.frame_size) {
                        if chunk.len() == st.vad.config.frame_size {
                            let vad_state = st.vad.process_frame(chunk);
                            st.vad_frame_count += 1;
                            // 每 ~10 秒自动适应一次阈值（500帧 x 20ms = 10s）
                            if st.vad_frame_count % 500 == 0 {
                                st.vad.adapt_threshold();
                            }
                            if vad_state == VadState::SpeechEnded {
                                speech_ended = true;
                            }
                        }
                    }

                    let Some(app) = app_weak.upgrade() else {
                        return;
                    };
                    match st.vad.state() {
                        VadState::Speaking => app.set_vad_state("speaking".into()),
                        VadState::SpeechEnded => app.set_vad_state("speech_ended".into()),
                        VadState::Silent => app.set_vad_state("silent".into()),
                    }

                    if !speech_ended {
                        return;
                    }

                    // Speech ended — stop recording and process
                    let engine = st.engine.clone();
                    let recorder = st.recorder.take();
                    let auto_speak = st.auto_speak;
                    st.recording_started = None;
                    st.vad_active = false;
                    app.set_is_busy(true);
                    app.set_status_text("处理中...".into());
                    app.set_status_type("processing".into());

                    let Some(engine) = engine else {
                        app.set_status_text("引擎未初始化".into());
                        app.set_status_type("error".into());
                        app.set_is_busy(false);
                        return;
                    };
                    let Some(recorder) = recorder else {
                        app.set_is_busy(false);
                        return;
                    };

                    let audio = match recorder.into_wav_bytes() {
                        Ok(a) => a,
                        Err(e) => {
                            app.set_status_text(slint::format!("录音失败: {}", e));
                            app.set_status_type("error".into());
                            app.set_is_busy(false);
                            return;
                        }
                    };

                    // Desktop captures the active screen; Android currently sends text-only input.
                    let image_data: Option<Vec<u8>> =
                        st.platform.as_ref().and_then(|p| p.capture_image());

                    drop(st);

                    #[cfg(target_os = "android")]
                    {
                        app.set_transcription("语音已发送到桌面端".into());
                        let wav_base64 = base64::engine::general_purpose::STANDARD.encode(&audio);
                        handle_remote_command(&state, &app, CommandInput::AudioWav { wav_base64 })
                            .await;
                        app.set_is_busy(false);
                        let mut st = state.lock().await;
                        start_continuous_listening(&mut st, &app);
                    }
                    #[cfg(not(target_os = "android"))]
                    {
                        // Transcribe audio
                        let transcription = {
                            let eng = engine.lock().await;
                            eng.transcribe(&audio).await
                        };

                        let Some(app) = app_weak.upgrade() else {
                            return;
                        };

                        let question = match transcription {
                            Ok(ref text) => {
                                app.set_transcription(text.clone().into());
                                text.clone()
                            }
                            Err(ref e) => {
                                app.set_transcription(slint::format!("转写失败: {}", e));
                                app.set_is_busy(false);
                                app.set_status_text("就绪".into());
                                app.set_status_type("ready".into());
                                let mut st = state.lock().await;
                                start_continuous_listening(&mut st, &app);
                                return;
                            }
                        };

                        handle_question_response(
                            &state,
                            &app,
                            &app_weak,
                            engine.clone(),
                            question,
                            image_data,
                            auto_speak,
                        )
                        .await;

                        app.set_is_busy(false);

                        // Restart listening
                        let mut st = state.lock().await;
                        start_continuous_listening(&mut st, &app);
                    }
                });
            },
        );
    }

    // Text submitted
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_text_submitted(move |text| {
            let question: String = text.into();
            if question.trim().is_empty() || question.chars().count() > 2_000 {
                return;
            }
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                let st = state.lock().await;
                let engine = st.engine.clone();
                let auto_speak = st.auto_speak;
                drop(st);

                let Some(engine) = engine else { return };

                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                app.set_transcription(question.clone().into());
                app.set_is_busy(true);
                app.set_status_text("分析中...".into());
                app.set_status_type("processing".into());

                // Get screen image
                let image_data = {
                    let st = state.lock().await;
                    st.platform.as_ref().and_then(|p| p.capture_image())
                };

                #[cfg(target_os = "android")]
                {
                    handle_remote_command(&state, &app, CommandInput::Text { text: question })
                        .await;
                }
                #[cfg(not(target_os = "android"))]
                {
                    handle_question_response(
                        &state,
                        &app,
                        &app_weak,
                        engine.clone(),
                        question,
                        image_data,
                        auto_speak,
                    )
                    .await;
                }

                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                app.set_is_busy(false);
            });
        });
    }

    // Confirm action
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_confirm_action(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                let mut st = state.lock().await;
                #[cfg(target_os = "android")]
                {
                    let client = st.remote_client.clone();
                    let request_id = st.pending_remote_request_id.take();
                    drop(st);
                    match (client, request_id) {
                        (Some(client), Some(request_id)) => {
                            match client.confirm(request_id, true).await {
                                Ok(status) => {
                                    let Some(app) = app_weak.upgrade() else {
                                        return;
                                    };
                                    app.set_show_confirmation(false);
                                    app.set_status_text(status.message.into());
                                    app.set_status_type("ready".into());
                                }
                                Err(error) => {
                                    let Some(app) = app_weak.upgrade() else {
                                        return;
                                    };
                                    app.set_show_confirmation(false);
                                    app.set_status_text(slint::format!("远程执行失败: {}", error));
                                    app.set_status_type("error".into());
                                }
                            }
                        }
                        _ => {
                            let Some(app) = app_weak.upgrade() else {
                                return;
                            };
                            app.set_show_confirmation(false);
                            app.set_status_text("未连接桌面端或没有待确认请求".into());
                            app.set_status_type("error".into());
                        }
                    }
                }
                #[cfg(not(target_os = "android"))]
                {
                    if let Some(plan) = st.pending_plan.take() {
                        // 统一的平台执行 — 不再需要 #[cfg] 分支
                        if let Some(ref platform) = st.platform {
                            if !platform.is_automation_ready() {
                                let Some(app) = app_weak.upgrade() else {
                                    return;
                                };
                                app.set_show_confirmation(false);
                                app.set_status_text("自动化引擎不可用".into());
                                app.set_status_type("error".into());
                            } else {
                                audit::record("approved", "local", &plan, None);
                                match platform.execute_plan(&plan, true) {
                                    Ok(result) => {
                                        audit::record("completed", "local", &plan, None);
                                        let Some(app) = app_weak.upgrade() else {
                                            return;
                                        };
                                        app.set_show_confirmation(false);
                                        app.set_status_text(slint::format!(
                                            "执行完成: {} 步",
                                            result.actions_executed
                                        ));
                                    }
                                    Err(e) => {
                                        audit::record(
                                            "failed",
                                            "local",
                                            &plan,
                                            Some(&e.to_string()),
                                        );
                                        let Some(app) = app_weak.upgrade() else {
                                            return;
                                        };
                                        app.set_show_confirmation(false);
                                        app.set_status_text(slint::format!("执行失败: {}", e));
                                        app.set_status_type("error".into());
                                    }
                                }
                            }
                        } else {
                            let Some(app) = app_weak.upgrade() else {
                                return;
                            };
                            app.set_show_confirmation(false);
                            app.set_status_text("平台服务未初始化".into());
                            app.set_status_type("error".into());
                        }
                    }
                } // end #[cfg(not(target_os = "android"))]
            });
        });
    }

    // Cancel action
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_cancel_action(move || {
            let state = state.clone();
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            spawn_local_task(async move {
                if let Some(plan) = state.lock().await.pending_plan.take() {
                    audit::record("cancelled", "local", &plan, None);
                }
            });
            app.set_show_confirmation(false);
            app.set_confirmation_text("".into());
            app.set_action_steps("".into());
        });
    }

    // Pause or resume local microphone monitoring.
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_toggle_listening(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                let mut st = state.lock().await;
                st.listening_enabled = !st.listening_enabled;
                if !st.listening_enabled {
                    st.recorder = None;
                    st.vad.reset();
                    st.vad_active = false;
                }
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                app.set_listening_enabled(st.listening_enabled);
                if st.listening_enabled {
                    start_continuous_listening(&mut st, &app);
                } else {
                    app.set_status_text("监听已暂停".into());
                    app.set_status_type("paused".into());
                }
            });
        });
    }

    // Open settings
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_open_settings(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                let st = state.lock().await;
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                if let Some(ref engine) = st.engine {
                    let eng = engine.lock().await;
                    apply_config_to_app(&app, eng.config());
                }
                app.set_show_settings(true);
            });
        });
    }

    // Close settings
    {
        let app_weak = app_weak.clone();
        app.on_close_settings(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_show_settings(false);
        });
    }

    // Settings field callbacks
    {
        let app_weak = app_weak.clone();
        app.on_provider_changed(move |text| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_provider(text);
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_api_key_changed(move |text| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_api_key(text);
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_api_url_changed(move |text| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_api_url(text);
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_model_changed(move |text| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_model(text);
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_wire_api_changed(move |value| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_wire_api(value);
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_reasoning_effort_changed(move |value| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_reasoning_effort(value);
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_store_responses_changed(move |value| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_store_responses(value);
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_max_tokens_changed(move |text| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_max_tokens_str(text);
        });
    }
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_auto_speak_changed(move |value| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_auto_speak(value);
            let state = state.clone();
            spawn_local_task(async move {
                state.lock().await.auto_speak = value;
            });
        });
    }
    {
        let app_weak = app_weak.clone();
        app.on_toggle_api_key_visible(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            app.set_show_api_key(!app.get_show_api_key());
        });
    }
    {
        #[cfg(target_os = "android")]
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_disconnect_remote(move || {
            #[cfg(target_os = "android")]
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                #[cfg(target_os = "android")]
                {
                    let mut st = state.lock().await;
                    st.remote_client = None;
                    st.pending_remote_request_id = None;
                }
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                app.set_remote_connected(false);
                app.set_remote_status("未连接桌面端".into());
            });
        });
    }

    // Save settings
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_save_settings(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                let st = state.lock().await;
                let engine = st.engine.clone();
                drop(st);

                let Some(engine) = engine else { return };
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                let config = {
                    let engine = engine.lock().await;
                    config_from_app(&app, engine.config())
                };

                app.set_is_busy(true);

                let result = save_settings(engine, config).await;
                let mut st = state.lock().await;
                let Some(app) = app_weak.upgrade() else {
                    return;
                };

                match result {
                    Ok((new_engine, new_config)) => {
                        st.engine = Some(new_engine);
                        // 应用弱语音模式 VAD 配置
                        if new_config.asr.weak_voice_mode {
                            let weak_vad = VadConfig::weak_voice();
                            st.vad = VoiceActivityDetector::new(weak_vad);
                        } else {
                            st.vad = VoiceActivityDetector::with_default_config();
                        }
                        // 重新创建平台服务。
                        let platform = platform::create_platform();
                        let capabilities = platform.capabilities();
                        app.set_capability_text(
                            format!(
                                "{}{}{}",
                                if capabilities.local_microphone {
                                    "麦克风 + "
                                } else {
                                    ""
                                },
                                if capabilities.image_capture {
                                    "视觉"
                                } else {
                                    "无本机视觉"
                                },
                                if capabilities.automation {
                                    " + 自动化"
                                } else {
                                    ""
                                }
                            )
                            .into(),
                        );
                        st.platform = Some(platform);
                        apply_config_to_app(&app, &new_config);
                        app.set_status_text("就绪".into());
                        app.set_status_type("ready".into());
                        app.set_show_settings(false);
                    }
                    Err(error) => {
                        app.set_status_text(slint::format!("保存失败: {}", error));
                        app.set_status_type("error".into());
                    }
                }
                app.set_is_busy(false);
            });
        });
    }

    // Test connection
    {
        let state = state.clone();
        let app_weak = app_weak.clone();
        app.on_test_connection(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            spawn_local_task(async move {
                let st = state.lock().await;
                let engine = st.engine.clone();
                drop(st);

                let Some(engine) = engine else { return };
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                let config = {
                    let engine = engine.lock().await;
                    config_from_app(&app, engine.config())
                };
                app.set_is_busy(true);

                let result = test_connection(&config).await;
                let Some(app) = app_weak.upgrade() else {
                    return;
                };

                match result {
                    Ok(true) => {
                        app.set_status_text("连接成功".into());
                        app.set_status_type("ready".into());
                    }
                    Ok(false) => {
                        app.set_status_text("连接失败".into());
                        app.set_status_type("error".into());
                    }
                    Err(e) => {
                        app.set_status_text(slint::format!("测试失败: {}", e));
                        app.set_status_type("error".into());
                    }
                }
                app.set_is_busy(false);
            });
        });
    }
}

fn spawn_local_task(future: impl Future<Output = ()> + 'static) {
    if let Err(error) = slint::spawn_local(future) {
        tracing::warn!("Failed to spawn UI task: {}", error);
    }
}

fn start_continuous_listening(st: &mut AppState, app: &AppWindow) {
    if !st.listening_enabled || st.recorder.is_some() {
        return;
    }
    match audio::Recorder::start() {
        Ok(recorder) => {
            st.recorder = Some(recorder);
            st.recording_started = Some(Instant::now());
            st.vad_sample_offset = 0;
            st.vad.reset();
            st.vad_frame_count = 0;
            st.vad_active = true;
            app.set_vad_state("silent".into());
            app.set_listening_enabled(true);
            app.set_status_text("监听中".into());
            app.set_status_type("listening".into());
        }
        Err(e) => {
            app.set_status_text(slint::format!("麦克风启动失败: {}", e));
            app.set_status_type("error".into());
        }
    }
}

fn apply_config_to_app(app: &AppWindow, config: &AppConfig) {
    app.set_provider(config.cloud_api.provider.clone().into());
    app.set_api_key(config.cloud_api.api_key.clone().into());
    app.set_api_url(config.cloud_api.api_url.clone().into());
    app.set_model(config.cloud_api.model.clone().into());
    app.set_wire_api(config.cloud_api.wire_api.clone().into());
    app.set_reasoning_effort(config.cloud_api.reasoning_effort.clone().into());
    app.set_store_responses(config.cloud_api.store_responses);
    app.set_max_tokens_str(config.cloud_api.max_tokens.to_string().into());
    app.set_auto_speak(config.ui.auto_speak);
}

#[cfg(target_os = "android")]
async fn handle_remote_command(state: &Arc<Mutex<AppState>>, app: &AppWindow, input: CommandInput) {
    let client = { state.lock().await.remote_client.clone() };
    let Some(client) = client else {
        app.set_ai_response("请先在设置中连接桌面端".into());
        app.set_status_text("未连接桌面端".into());
        app.set_status_type("error".into());
        return;
    };

    match client.send_command(input).await {
        Ok(preview) => {
            app.set_ai_response(preview.response_text.into());
            app.set_action_steps(preview.action_steps.join("\n").into());
            if preview.has_plan {
                state.lock().await.pending_remote_request_id = Some(preview.request_id);
                app.set_confirmation_text(preview.confirmation_text.into());
                app.set_show_confirmation(true);
            } else {
                app.set_show_confirmation(false);
            }
            app.set_status_text("桌面端已返回计划".into());
            app.set_status_type("ready".into());
        }
        Err(error) => {
            app.set_ai_response(slint::format!("远程请求失败: {}", error));
            app.set_status_text("远程请求失败".into());
            app.set_status_type("error".into());
        }
    }
}

fn config_from_app(app: &AppWindow, base: &AppConfig) -> AppConfig {
    let mut config = base.clone();
    config.cloud_api.provider = app.get_provider().to_string();
    config.cloud_api.api_key = app.get_api_key().to_string();
    config.cloud_api.api_url = app
        .get_api_url()
        .to_string()
        .trim_end_matches('/')
        .to_string();
    config.cloud_api.model = app.get_model().to_string();
    config.cloud_api.wire_api = app.get_wire_api().to_string();
    config.cloud_api.reasoning_effort = app.get_reasoning_effort().to_string();
    config.cloud_api.store_responses = app.get_store_responses();
    if let Ok(budget) = app.get_max_tokens_str().to_string().parse::<usize>() {
        config.cloud_api.max_tokens = budget;
    }
    config.ui.auto_speak = app.get_auto_speak();
    config
}

async fn create_engine() -> Result<(Arc<Mutex<AleEngine>>, AppConfig), String> {
    let engine = AleEngineFactory::create_default()
        .await
        .map_err(|error| error.to_string())?;
    let config = engine.config().clone();
    Ok((Arc::new(Mutex::new(engine)), config))
}

async fn save_settings(
    engine: Arc<Mutex<AleEngine>>,
    config: AppConfig,
) -> Result<(Arc<Mutex<AleEngine>>, AppConfig), String> {
    {
        let mut engine = engine.lock().await;
        engine
            .update_config(config)
            .map_err(|error| error.to_string())?;
    }
    create_engine().await
}

async fn test_connection(config: &AppConfig) -> Result<bool, String> {
    ensure_api_key(config)?;
    AleEngine::test_cloud_api_config(&config.cloud_api)
        .await
        .map_err(|error| error.to_string())
}

fn ensure_api_key(config: &AppConfig) -> Result<(), String> {
    if config.cloud_api.api_key.trim().is_empty() {
        return Err("API key 未配置".to_string());
    }
    Ok(())
}
