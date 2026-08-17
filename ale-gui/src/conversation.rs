use crate::platform::CapturedImage;
use crate::{audit, tts_player, AppState, AppWindow};
use ale_core::actions::parse_action_plan_arguments;
use ale_core::cloud::ToolCall;
use ale_core::AleEngine;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::Mutex;

struct AssistantReply {
    content: String,
    tokens_used: usize,
    tool_calls: Option<Vec<ToolCall>>,
}

pub async fn handle_question_response(
    state: &Rc<Mutex<AppState>>,
    app: &AppWindow,
    app_weak: &slint::Weak<AppWindow>,
    engine: Arc<Mutex<AleEngine>>,
    question: String,
    image_data: Option<CapturedImage>,
    auto_speak: bool,
) {
    let result = ask_question(app, engine.clone(), &question, image_data.as_ref()).await;

    match result {
        Ok(reply) => {
            app.set_ai_response(reply.content.clone().into());
            record_interaction(
                app,
                engine.clone(),
                &question,
                &reply.content,
                reply.tokens_used,
            )
            .await;

            if let Some(calls) = reply.tool_calls {
                apply_tool_calls(state, app, &calls, image_data.as_ref()).await;
            } else {
                state.lock().await.pending_plan = None;
                app.set_action_steps("".into());
                app.set_confirmation_text("".into());
                app.set_show_confirmation(false);
            }

            app.set_status_text("就绪".into());
            app.set_status_type("ready".into());

            if auto_speak && !reply.content.is_empty() {
                let app_weak = app_weak.clone();
                let text = reply.content;
                spawn_local_task(async move {
                    let _ = speak_and_play(engine, &text).await;
                    let Some(app) = app_weak.upgrade() else {
                        return;
                    };
                    app.set_status_text("就绪".into());
                    app.set_status_type("ready".into());
                });
            }
        }
        Err(error) => {
            app.set_ai_response(slint::format!("失败: {}", error));
            app.set_status_text("就绪".into());
            app.set_status_type("ready".into());
        }
    }
}

fn spawn_local_task(future: impl Future<Output = ()> + 'static) {
    if let Err(error) = slint::spawn_local(future) {
        tracing::warn!("Failed to spawn UI task: {}", error);
    }
}

async fn ask_question(
    app: &AppWindow,
    engine: Arc<Mutex<AleEngine>>,
    question: &str,
    image_data: Option<&CapturedImage>,
) -> Result<AssistantReply, String> {
    if let Some(image_data) = image_data {
        app.set_status_text("分析画面...".into());
        let response = {
            let engine = engine.lock().await;
            engine
                .ask_about_image(&image_data.jpeg_data, question)
                .await
                .map_err(|error| error.to_string())?
        };

        return Ok(AssistantReply {
            content: response.content,
            tokens_used: response.tokens_used,
            tool_calls: None,
        });
    }

    app.set_status_text("思考中...".into());
    let response = {
        let engine = engine.lock().await;
        engine
            .ask_text(question)
            .await
            .map_err(|error| error.to_string())?
    };

    Ok(AssistantReply {
        content: response.content,
        tokens_used: response.tokens_used,
        tool_calls: None,
    })
}

async fn record_interaction(
    app: &AppWindow,
    engine: Arc<Mutex<AleEngine>>,
    question: &str,
    answer: &str,
    tokens_used: usize,
) {
    let mut engine = engine.lock().await;
    let ctx = engine.context_mut();
    ctx.add_user_message(question.to_string());
    ctx.add_assistant_message(answer.to_string());
    ctx.add_tokens(tokens_used);
    app.set_session_tokens(ctx.session_tokens() as i32);
    let _ = engine.learn_from_interaction(question, answer);
}

async fn apply_tool_calls(
    state: &Rc<Mutex<AppState>>,
    app: &AppWindow,
    calls: &[ToolCall],
    captured_image: Option<&CapturedImage>,
) {
    let executable = calls
        .iter()
        .filter(|call| call.function.name == "execute_action_plan")
        .collect::<Vec<_>>();
    if executable.len() != 1 {
        state.lock().await.pending_plan = None;
        app.set_action_steps("模型返回了无效的自动化计划".into());
        app.set_confirmation_text("".into());
        app.set_show_confirmation(false);
        return;
    }

    let plan = match parse_action_plan_arguments(&executable[0].function.arguments) {
        Ok(plan) => match captured_image {
            Some(capture) => capture
                .coordinate_space
                .map_plan(plan)
                .map_err(|error| error.to_string()),
            None => Err("自动化计划缺少对应的屏幕坐标信息".to_string()),
        },
        Err(error) => Err(error.to_string()),
    };

    if let Ok(plan) = plan {
        app.set_action_steps(plan.describe_steps().join("\n").into());
        let confirmation_text = plan.speak_text();
        audit::record("created", "local", &plan, None);
        state.lock().await.pending_plan = Some(plan);
        app.set_confirmation_text(confirmation_text.into());
        app.set_show_confirmation(true);
    } else {
        state.lock().await.pending_plan = None;
        app.set_action_steps("自动化坐标无效或已超出截图范围".into());
        app.set_confirmation_text("".into());
        app.set_show_confirmation(false);
    }
}

pub(crate) fn automation_tools() -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "execute_action_plan",
            "description": "Create a desktop automation plan for the visible screen. The app will show the plan to the user for confirmation before executing it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "explanation": {
                        "type": "string",
                        "description": "Short natural-language explanation of what will be done."
                    },
                    "risk_level": {
                        "type": "string",
                        "enum": ["low", "medium", "high"]
                    },
                    "requires_confirmation": {
                        "type": "boolean",
                        "description": "Whether the user must confirm before execution. Use true for any action that changes data, opens apps, types, clicks, or uses files."
                    },
                    "actions": {
                        "type": "array",
                        "items": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["click"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" },
                                        "button": { "type": "string", "enum": ["left", "right", "middle"] }
                                    },
                                    "required": ["type", "x", "y", "button"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["double_click"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" }
                                    },
                                    "required": ["type", "x", "y"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["mouse_move"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" }
                                    },
                                    "required": ["type", "x", "y"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["scroll"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" },
                                        "delta_x": { "type": "number" },
                                        "delta_y": { "type": "number" }
                                    },
                                    "required": ["type", "x", "y", "delta_x", "delta_y"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["type"] },
                                        "text": { "type": "string" }
                                    },
                                    "required": ["type", "text"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["key"] },
                                        "key": { "type": "string" },
                                        "modifiers": { "type": "array", "items": { "type": "string" } }
                                    },
                                    "required": ["type", "key", "modifiers"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["wait"] },
                                        "ms": { "type": "integer" }
                                    },
                                    "required": ["type", "ms"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["open_app"] },
                                        "name": { "type": "string" }
                                    },
                                    "required": ["type", "name"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["open_url"] },
                                        "url": { "type": "string" }
                                    },
                                    "required": ["type", "url"]
                                }
                            ]
                        }
                    }
                },
                "required": ["explanation", "risk_level", "requires_confirmation", "actions"]
            }
        }
    })]
}

async fn speak_and_play(engine: Arc<Mutex<AleEngine>>, text: &str) -> Result<(), String> {
    let audio = {
        let engine = engine.lock().await;
        ensure_api_key(engine.config())?;
        engine
            .synthesize(text)
            .await
            .map_err(|error| error.to_string())?
    };

    tokio::task::spawn_blocking(move || tts_player::play_audio(&audio))
        .await
        .map_err(|error| format!("播放失败: {error}"))?
}

fn ensure_api_key(config: &ale_core::config::AppConfig) -> Result<(), String> {
    if config.cloud_api.api_key.trim().is_empty() {
        return Err("API key 未配置".to_string());
    }
    Ok(())
}
