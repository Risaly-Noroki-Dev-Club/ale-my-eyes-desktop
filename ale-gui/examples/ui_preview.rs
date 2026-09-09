//! Deterministic native UI fixtures; no engine, microphone, credentials, or network services.
use ale_gui::{AppWindow, DeviceRow, LogRow, ModelRow, Ui};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = AppWindow::new()?;
    let ui = app.global::<Ui>();
    ui.set_ready(true);
    ui.set_connected(true);
    ui.set_pairing_code("JVVDPV".into());
    ui.set_remaining(119);
    ui.set_pairing_status("等待手机连接".into());
    ui.set_pairing_uri("ale-my-eyes://pair?host=192.168.1.42&port=37654&code=JVVDPV".into());
    let code = qrcode::QrCode::new(b"ale-my-eyes://preview-only")?;
    let qr = code
        .render::<image::Rgba<u8>>()
        .min_dimensions(192, 192)
        .build();
    let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(qr.width(), qr.height());
    pixels.make_mut_bytes().copy_from_slice(qr.as_raw());
    ui.set_qr(slint::Image::from_rgba8(pixels));
    ui.set_devices(ModelRc::new(VecModel::from(vec![DeviceRow {
        id: "preview".into(),
        name: "手机端".into(),
    }])));
    ui.set_device_name("手机端".into());
    ui.set_device_address("192.168.1.87".into());
    ui.set_latency("42 ms".into());
    ui.set_task("分析当前可交互元素…".into());
    ui.set_decision_kind("高风险操作 · 待确认".into());
    ui.set_decision(
        "即将点击「确认支付 ¥128.00」按钮。\n请核对收款方、金额与订单内容，确认后才会执行。".into(),
    );
    ui.set_can_confirm(true);
    ui.set_models(ModelRc::new(VecModel::from(
        ["SenseVoiceSmall", "Qwen2.5-VL", "ShowUI"]
            .map(|name| ModelRow {
                name: name.into(),
                state: "可用".into(),
            })
            .to_vec(),
    )));
    ui.set_logs(ModelRc::new(VecModel::from(
        (0..12)
            .map(|i| LogRow {
                time: format!("14:32:{i:02}").into(),
                message: "定位可交互元素".into(),
                detail: "preview · Grounding".into(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_api_url("https://api.openai.com/v1".into());
    ui.set_model("gpt-4o".into());
    ui.set_api_key("preview-only-not-a-secret".into());
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.set_build_date("2026-09-09".into());
    let weak = app.as_weak();
    ui.on_navigate(move |page| {
        if let Some(app) = weak.upgrade() {
            app.global::<Ui>().set_page(page);
        }
    });
    let weak = app.as_weak();
    ui.on_pause(move || {
        if let Some(app) = weak.upgrade() {
            let ui = app.global::<Ui>();
            ui.set_paused(!ui.get_paused());
        }
    });
    let weak = app.as_weak();
    ui.on_decide(move |_| {
        if let Some(app) = weak.upgrade() {
            app.global::<Ui>().set_decision("".into());
        }
    });
    let args = std::env::args().collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--interactive") {
        return Ok(app.run()?);
    }
    if args.iter().any(|arg| arg == "--model-settings") {
        return model_settings_preview(app);
    }
    std::fs::create_dir_all("target/ui-preview")?;
    let tick = Rc::new(Cell::new(0usize));
    let weak = app.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(400),
        move || {
            let index = tick.get();
            let Some(app) = weak.upgrade() else { return };
            let cases = [
                (1032, 800, false, false),
                (1280, 900, false, false),
                (440, 640, false, false),
                (1032, 800, true, true),
                (440, 640, true, true),
            ];
            let case = index / 12;
            if case >= cases.len() {
                slint::quit_event_loop().unwrap();
                return;
            }
            let page = ["pairing", "main", "settings"][(index / 4) % 3];
            let (width, height, english, contrast) = cases[case];
            if index.is_multiple_of(4) {
                app.window()
                    .set_size(slint::LogicalSize::new(width as f32, height as f32));
                // Recreate the page between cases so a previous wheel gesture or
                // focused control cannot carry its scroll offset into a first-screen fixture.
                app.global::<Ui>().set_page("".into());
                let weak = app.as_weak();
                slint::Timer::single_shot(Duration::from_millis(100), move || {
                    if let Some(app) = weak.upgrade() {
                        app.global::<Ui>().set_page(page.into());
                    }
                });
                app.global::<Ui>().set_english(english);
                app.global::<Ui>().set_high_contrast(contrast);
                app.global::<Ui>()
                    .set_theme(if contrast { "dark" } else { "light" }.into());
                app.global::<Ui>()
                    .set_pairing_status(if english { "Connected" } else { "已连接" }.into());
            } else if index % 4 == 2 {
                app.window()
                    .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                        position: slint::LogicalPosition::new(30.0, 400.0),
                        delta_x: 0.0,
                        delta_y: -2000.0,
                    });
            } else {
                match app.window().take_snapshot() {
                    Ok(image) => {
                        image::save_buffer(
                            format!(
                                "target/ui-preview/{page}-{width}-{height}-{english}{}.png",
                                if index % 4 == 3 { "-bottom" } else { "" }
                            ),
                            image.as_bytes(),
                            image.width(),
                            image.height(),
                            image::ColorType::Rgba8,
                        )
                        .unwrap();
                    }
                    Err(error) => {
                        eprintln!("snapshot failed: {error}");
                        slint::quit_event_loop().unwrap();
                    }
                }
            }
            tick.set(index + 1);
        },
    );
    app.run()?;
    Ok(())
}

fn model_settings_preview(app: AppWindow) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all("target/ui-preview")?;
    let ui = app.global::<Ui>();
    ui.set_advanced(true);
    ui.set_backup_enabled(true);
    ui.set_backup_authorized(true);
    ui.set_backup_wire_api(2);
    ui.set_backup_url("https://api.anthropic.com/v1".into());
    ui.set_backup_model("claude-sonnet-4-20250514".into());
    ui.set_asr_enabled(true);
    ui.set_testing(true);
    ui.set_test_report("Primary / Text: Passed (402 ms)\nPrimary / Image: Passed (820 ms)\nPrimary / Text + tools: Passed (502 ms)\nPrimary / Image + tools: Testing...".into());
    ui.set_test_targets("Primary: gpt-4o\nhttps://api.openai.com/v1\n4 requests\nBackup: claude-sonnet-4-20250514\nhttps://api.anthropic.com/v1\n4 requests\nTranscription: whisper-1\nhttps://api.openai.com/v1\n1 request".into());
    let tick = Rc::new(Cell::new(0usize));
    let weak = app.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(500),
        move || {
            let index = tick.get();
            let Some(app) = weak.upgrade() else { return };
            let cases = [
                (1032, 800, false),
                (440, 640, false),
                (1032, 800, true),
                (440, 640, true),
            ];
            let case = index / 8;
            if case >= cases.len() {
                slint::quit_event_loop().unwrap();
                return;
            }
            let (width, height, en) = cases[case];
            let ui = app.global::<Ui>();
            match index % 8 {
                0 => {
                    ui.set_test_prompt(false);
                    ui.set_page("".into());
                    ui.set_english(en);
                    ui.set_high_contrast(en);
                    ui.set_theme(if en { "dark" } else { "light" }.into());
                    app.window()
                        .set_size(slint::LogicalSize::new(width as f32, height as f32));
                    let weak = app.as_weak();
                    slint::Timer::single_shot(Duration::from_millis(100), move || {
                        if let Some(app) = weak.upgrade() {
                            app.global::<Ui>().set_page("settings".into());
                        }
                    });
                }
                2 | 4 => {
                    app.window()
                        .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                            position: slint::LogicalPosition::new(width as f32 / 2.0, 400.0),
                            delta_x: 0.0,
                            delta_y: -650.0,
                        })
                }
                6 => ui.set_test_prompt(true),
                _ => {
                    let snapshot = app.window().take_snapshot().unwrap();
                    image::save_buffer(
                        format!("target/ui-preview/models-{width}-{en}-{}.png", index % 8),
                        snapshot.as_bytes(),
                        snapshot.width(),
                        snapshot.height(),
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                }
            }
            tick.set(index + 1);
        },
    );
    app.run()?;
    Ok(())
}
