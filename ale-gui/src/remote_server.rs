use crate::audit;
use crate::conversation::automation_tools;
use crate::platform::{self, PlatformService};
use crate::remote_crypto;
use ale_core::actions::{parse_action_plan_arguments, ActionPlan};
use ale_core::remote::{
    ClientHello, CommandInput, CommandPreview, ConfirmExecution, ExecutionState, ExecutionStatus,
    PairingInfo, RemoteError, RemoteMessage, ServerHello, DEFAULT_REMOTE_PORT,
    REMOTE_PROTOCOL_VERSION,
};
use ale_core::AleEngine;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use local_ip_address::local_ip;
use qrcode::types::Color;
use qrcode::QrCode;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

pub struct RemoteServerHandle {
    pub qr_image: slint::Image,
}

pub async fn start(engine: Arc<Mutex<AleEngine>>) -> Result<RemoteServerHandle, String> {
    let code = remote_crypto::pairing_code();
    let session_id = remote_crypto::session_id();
    let name = remote_crypto::device_name();
    let host = local_ip()
        .map_err(|_| "未找到可用于移动端连接的局域网地址".to_string())?
        .to_string();
    let pairing = PairingInfo {
        host,
        port: DEFAULT_REMOTE_PORT,
        session_id,
        code,
        name,
    };
    let qr_image = render_qr(&pairing.uri())?;

    let listener = TcpListener::bind(("0.0.0.0", DEFAULT_REMOTE_PORT))
        .await
        .map_err(|error| error.to_string())?;
    let pending = Arc::new(Mutex::new(HashMap::<String, ActionPlan>::new()));
    let platform: Arc<dyn PlatformService> = Arc::from(platform::create_platform());
    let server_pairing = pairing.clone();

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let engine = engine.clone();
                    let pending = pending.clone();
                    let pairing = server_pairing.clone();
                    let platform = platform.clone();
                    tokio::spawn(async move {
                        if let Err(error) =
                            handle_connection(stream, addr, engine, pending, pairing, platform)
                                .await
                        {
                            tracing::warn!("Remote client disconnected: {}", error);
                        }
                    });
                }
                Err(error) => tracing::warn!("Remote accept failed: {}", error),
            }
        }
    });

    Ok(RemoteServerHandle { qr_image })
}

async fn handle_connection(
    stream: TcpStream,
    _addr: SocketAddr,
    engine: Arc<Mutex<AleEngine>>,
    pending: Arc<Mutex<HashMap<String, ActionPlan>>>,
    pairing: PairingInfo,
    platform: Arc<dyn PlatformService>,
) -> Result<(), String> {
    let mut socket = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(|error| error.to_string())?;

    let client_handshake = socket
        .next()
        .await
        .ok_or_else(|| "missing handshake".to_string())?
        .map_err(|error| error.to_string())?
        .into_data();
    let (mut secure, server_handshake) =
        remote_crypto::server_handshake_reply(&pairing.code, &client_handshake)?;
    socket
        .send(Message::Binary(server_handshake))
        .await
        .map_err(|error| error.to_string())?;

    send_secure(
        &mut socket,
        &mut secure,
        &RemoteMessage::ServerHello(ServerHello {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            device_name: pairing.name.clone(),
            session_id: pairing.session_id.clone(),
        }),
    )
    .await?;

    while let Some(frame) = socket.next().await {
        let frame = frame.map_err(|error| error.to_string())?;
        if !frame.is_binary() {
            continue;
        }
        let message = secure.decrypt_message(&frame.into_data())?;
        match message {
            RemoteMessage::ClientHello(ClientHello { .. }) => {}
            RemoteMessage::CommandRequest(request) => {
                let request_id = request.request_id.clone();
                match handle_command(
                    engine.clone(),
                    platform.clone(),
                    &request.request_id,
                    &request.input,
                )
                .await
                {
                    Ok((preview, plan)) => {
                        if let Some(plan) = plan {
                            audit::record("created", "remote", &plan, None);
                            pending.lock().await.insert(request_id.clone(), plan);
                        }
                        send_secure(
                            &mut socket,
                            &mut secure,
                            &RemoteMessage::CommandPreview(preview),
                        )
                        .await?;
                    }
                    Err(error) => {
                        send_secure(
                            &mut socket,
                            &mut secure,
                            &RemoteMessage::Error(RemoteError {
                                request_id: Some(request_id),
                                message: error,
                            }),
                        )
                        .await?;
                    }
                }
            }
            RemoteMessage::ConfirmExecution(confirm) => {
                let status = handle_confirm(confirm, pending.clone(), platform.clone()).await;
                send_secure(
                    &mut socket,
                    &mut secure,
                    &RemoteMessage::ExecutionStatus(status),
                )
                .await?;
            }
            _ => {}
        }
    }

    Ok(())
}

async fn handle_command(
    engine: Arc<Mutex<AleEngine>>,
    platform: Arc<dyn PlatformService>,
    request_id: &str,
    input: &CommandInput,
) -> Result<(CommandPreview, Option<ActionPlan>), String> {
    let request_id = request_id.to_string();
    let question = match input {
        CommandInput::Text { text } => text.clone(),
        CommandInput::AudioWav { wav_base64 } => {
            let audio = base64::engine::general_purpose::STANDARD
                .decode(wav_base64)
                .map_err(|error| error.to_string())?;
            let engine = engine.lock().await;
            engine
                .transcribe(&audio)
                .await
                .map_err(|error| error.to_string())?
        }
    };

    let image = platform.capture_image();
    let response = if let Some(image) = image {
        let engine = engine.lock().await;
        engine
            .ask_about_image_with_tools(&image, &question, automation_tools())
            .await
            .map_err(|error| error.to_string())?
    } else {
        let engine = engine.lock().await;
        let response = engine
            .ask_text(&question)
            .await
            .map_err(|error| error.to_string())?;
        return Ok((
            CommandPreview {
                request_id,
                response_text: response.content,
                action_steps: Vec::new(),
                confirmation_text: String::new(),
                requires_confirmation: false,
                has_plan: false,
            },
            None,
        ));
    };

    let mut action_steps = Vec::new();
    let mut plan = None;
    if let Some(calls) = response.tool_calls {
        let executable = calls
            .iter()
            .filter(|call| call.function.name == "execute_action_plan")
            .collect::<Vec<_>>();
        if executable.len() == 1 {
            if let Ok(parsed) = parse_action_plan_arguments(&executable[0].function.arguments) {
                action_steps = parsed.describe_steps();
                plan = Some(parsed);
            }
        }
    }

    let confirmation_text = plan
        .as_ref()
        .map(ActionPlan::speak_text)
        .unwrap_or_default();
    let requires_confirmation = plan
        .as_ref()
        .map(|plan| plan.requires_confirmation)
        .unwrap_or(false);
    let has_plan = plan.is_some();

    Ok((
        CommandPreview {
            request_id,
            response_text: response.content,
            action_steps,
            confirmation_text,
            requires_confirmation,
            has_plan,
        },
        plan,
    ))
}

async fn handle_confirm(
    confirm: ConfirmExecution,
    pending: Arc<Mutex<HashMap<String, ActionPlan>>>,
    platform: Arc<dyn PlatformService>,
) -> ExecutionStatus {
    if !confirm.approved {
        if let Some(plan) = pending.lock().await.remove(&confirm.request_id) {
            audit::record("cancelled", "remote", &plan, None);
        }
        return ExecutionStatus {
            request_id: confirm.request_id,
            state: ExecutionState::Cancelled,
            message: "已取消".to_string(),
            actions_executed: 0,
        };
    }

    let Some(plan) = pending.lock().await.remove(&confirm.request_id) else {
        return ExecutionStatus {
            request_id: confirm.request_id,
            state: ExecutionState::Failed,
            message: "找不到待执行计划".to_string(),
            actions_executed: 0,
        };
    };

    audit::record("approved", "remote", &plan, None);
    match platform.execute_plan(&plan, true) {
        Ok(result) => {
            audit::record("completed", "remote", &plan, None);
            ExecutionStatus {
                request_id: confirm.request_id,
                state: ExecutionState::Completed,
                message: format!("执行完成: {} 步", result.actions_executed),
                actions_executed: result.actions_executed,
            }
        }
        Err(error) => {
            audit::record("failed", "remote", &plan, Some(&error.to_string()));
            ExecutionStatus {
                request_id: confirm.request_id,
                state: ExecutionState::Failed,
                message: error.to_string(),
                actions_executed: 0,
            }
        }
    }
}

async fn send_secure(
    socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    secure: &mut remote_crypto::SecureChannel,
    message: &RemoteMessage,
) -> Result<(), String> {
    let frame = secure.encrypt_message(message)?;
    socket
        .send(Message::Binary(frame))
        .await
        .map_err(|error| error.to_string())
}

fn render_qr(uri: &str) -> Result<slint::Image, String> {
    let code = QrCode::new(uri.as_bytes()).map_err(|error| error.to_string())?;
    const QUIET_ZONE: usize = 4;
    const SCALE: usize = 6;
    let module_width = code.width();
    let image_width = (module_width + QUIET_ZONE * 2) * SCALE;
    let mut pixels =
        slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(image_width as u32, image_width as u32);
    let bytes = pixels.make_mut_bytes();
    bytes.fill(255);
    for (index, color) in code.to_colors().into_iter().enumerate() {
        if color != Color::Dark {
            continue;
        }
        let module_x = index % module_width;
        let module_y = index / module_width;
        for y in 0..SCALE {
            for x in 0..SCALE {
                let pixel_x = (module_x + QUIET_ZONE) * SCALE + x;
                let pixel_y = (module_y + QUIET_ZONE) * SCALE + y;
                let offset = (pixel_y * image_width + pixel_x) * 3;
                bytes[offset..offset + 3].fill(0);
            }
        }
    }
    Ok(slint::Image::from_rgb8(pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_qr_decodes_to_original_uri() {
        let uri = format!(
            "ale-my-eyes://pair?host=192.168.1.2&port=37654&sid={}&code=123456&name=Desktop",
            uuid::Uuid::new_v4()
        );
        let image = render_qr(&uri).unwrap().to_rgb8().unwrap();
        let gray = image
            .as_bytes()
            .chunks_exact(3)
            .map(|pixel| pixel[0])
            .collect::<Vec<_>>();
        let mut decoder = quircs::Quirc::default();
        let decoded = decoder
            .identify(image.width() as usize, image.height() as usize, &gray)
            .find_map(|code| code.ok()?.decode().ok())
            .expect("rendered QR should decode");
        assert_eq!(decoded.payload, uri.as_bytes());
    }
}
