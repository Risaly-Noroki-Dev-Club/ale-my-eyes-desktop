use ale_core::actions::{Action, ActionPlan};
use ale_core::{AleError, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

static CAPTURE_VISIBILITY_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 屏幕帧数据
#[derive(Clone)]
pub struct ScreenFrame {
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub desktop_x: i32,
    pub desktop_y: i32,
    pub desktop_width: u32,
    pub desktop_height: u32,
    pub scale_factor: f32,
    pub rgba_data: Vec<u8>,
    pub timestamp: Instant,
    capture_generation: u64,
}

#[derive(Clone, Debug)]
pub struct CapturedScreen {
    pub jpeg_data: Vec<u8>,
    pub coordinate_space: ScreenCoordinateSpace,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenCoordinateSpace {
    pub image_width: u32,
    pub image_height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub desktop_x: i32,
    pub desktop_y: i32,
    pub desktop_width: u32,
    pub desktop_height: u32,
    pub scale_factor: f32,
}

impl ScreenCoordinateSpace {
    pub fn map_point(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        if !x.is_finite() || !y.is_finite() {
            return Err(AleError::ConfigError("自动化坐标必须是有限数".to_string()));
        }
        if self.image_width == 0
            || self.image_height == 0
            || self.desktop_width == 0
            || self.desktop_height == 0
        {
            return Err(AleError::ConfigError("屏幕坐标空间无效".to_string()));
        }

        let image_max_x = self.image_width.saturating_sub(1) as f64;
        let image_max_y = self.image_height.saturating_sub(1) as f64;
        if x < 0.0 || y < 0.0 || x > image_max_x || y > image_max_y {
            return Err(AleError::ConfigError(format!(
                "自动化坐标 ({x}, {y}) 超出截图范围 {}x{}",
                self.image_width, self.image_height
            )));
        }

        let desktop_span_x = self.desktop_width.saturating_sub(1) as f64;
        let desktop_span_y = self.desktop_height.saturating_sub(1) as f64;
        let mapped_x = self.desktop_x as f64
            + if image_max_x == 0.0 {
                0.0
            } else {
                x / image_max_x * desktop_span_x
            };
        let mapped_y = self.desktop_y as f64
            + if image_max_y == 0.0 {
                0.0
            } else {
                y / image_max_y * desktop_span_y
            };
        Ok((mapped_x, mapped_y))
    }

    pub fn map_plan(&self, mut plan: ActionPlan) -> Result<ActionPlan> {
        for action in &mut plan.actions {
            let coordinates = match action {
                Action::Click { x, y, .. }
                | Action::ControlledTestClick { x, y, .. }
                | Action::DoubleClick { x, y }
                | Action::MouseMove { x, y }
                | Action::Scroll { x, y, .. } => Some((x, y)),
                _ => None,
            };
            if let Some((x, y)) = coordinates {
                (*x, *y) = self.map_point(*x, *y)?;
            }
        }
        plan.validate()?;
        Ok(plan)
    }
}

impl ScreenFrame {
    pub fn coordinate_space(&self) -> ScreenCoordinateSpace {
        ScreenCoordinateSpace {
            image_width: self.width,
            image_height: self.height,
            source_width: self.source_width,
            source_height: self.source_height,
            desktop_x: self.desktop_x,
            desktop_y: self.desktop_y,
            desktop_width: self.desktop_width,
            desktop_height: self.desktop_height,
            scale_factor: self.scale_factor,
        }
    }
}

/// 屏幕捕获配置
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// 截图间隔
    pub interval: Duration,
    /// 缩放比例（0.0-1.0）
    pub scale: f32,
    /// JPEG 质量（用于发送给 API）
    pub jpeg_quality: u8,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3),
            scale: 0.5,
            jpeg_quality: 80,
        }
    }
}

/// 屏幕捕获器（Desktop only）
pub struct ScreenCapture {
    latest_frame: Arc<Mutex<Option<ScreenFrame>>>,
    running: Arc<Mutex<bool>>,
    config: CaptureConfig,
}

impl ScreenCapture {
    pub fn new(config: CaptureConfig) -> Self {
        Self {
            latest_frame: Arc::new(Mutex::new(None)),
            running: Arc::new(Mutex::new(false)),
            config,
        }
    }

    /// 开始持续捕获
    pub fn start(&self) -> Result<()> {
        let mut running = self
            .running
            .lock()
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to lock running flag: {}", e)))?;

        if *running {
            return Ok(());
        }
        *running = true;
        drop(running);

        let latest_frame = self.latest_frame.clone();
        let running = self.running.clone();
        let interval = self.config.interval;
        let scale = self.config.scale;

        thread::spawn(move || {
            while {
                let Ok(r) = running.lock() else {
                    tracing::warn!("Screen capture running flag lock poisoned");
                    return;
                };
                *r
            } {
                if capture_is_suspended() {
                    if let Ok(mut frame) = latest_frame.lock() {
                        *frame = None;
                    }
                    thread::sleep(interval);
                    continue;
                }
                match capture_primary_monitor(scale) {
                    Ok(frame) => {
                        if let Ok(mut lf) = latest_frame.lock() {
                            *lf = Some(frame);
                        } else {
                            tracing::warn!("Screen capture frame lock poisoned");
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Screen capture failed: {}", e);
                    }
                }
                thread::sleep(interval);
            }
        });

        Ok(())
    }

    /// 停止捕获
    pub fn stop(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = false;
        }
    }

    pub fn set_suspended(&self, suspended: bool) {
        set_capture_suspended(suspended);
        if let Ok(mut frame) = self.latest_frame.lock() {
            *frame = None;
        }
    }

    /// 获取最新帧
    pub fn latest_frame(&self) -> Option<ScreenFrame> {
        let generation = CAPTURE_VISIBILITY_GENERATION.load(Ordering::Acquire);
        if generation_is_suspended(generation) {
            return None;
        }
        self.latest_frame
            .lock()
            .ok()?
            .as_ref()
            .filter(|frame| frame.capture_generation == generation)
            .cloned()
    }

    /// 获取最新帧的 JPEG 数据（用于发送给 API）
    pub fn latest_frame_jpeg(&self) -> Option<Vec<u8>> {
        let frame = self.latest_frame()?;
        frame_to_jpeg(&frame, self.config.jpeg_quality).ok()
    }

    pub fn latest_capture(&self) -> Option<CapturedScreen> {
        let frame = self.latest_frame()?;
        Some(CapturedScreen {
            jpeg_data: frame_to_jpeg(&frame, self.config.jpeg_quality).ok()?,
            coordinate_space: frame.coordinate_space(),
        })
    }

    /// 立即截取一帧
    pub fn capture_now(&self) -> Result<ScreenFrame> {
        capture_primary_monitor(self.config.scale)
    }

    pub fn capture_now_jpeg(&self) -> Result<CapturedScreen> {
        let frame = self.capture_now()?;
        Ok(CapturedScreen {
            jpeg_data: frame_to_jpeg(&frame, self.config.jpeg_quality)?,
            coordinate_space: frame.coordinate_space(),
        })
    }
}

/// 捕获主显示器
fn capture_primary_monitor(scale: f32) -> Result<ScreenFrame> {
    let capture_generation = CAPTURE_VISIBILITY_GENERATION.load(Ordering::Acquire);
    if generation_is_suspended(capture_generation) {
        return Err(AleError::ConfigError(
            "敏感设置界面可见，屏幕捕获已暂停".to_string(),
        ));
    }
    let monitors = xcap::Monitor::all()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to enumerate monitors: {}", e)))?;

    let monitor = monitors
        .iter()
        .find(|monitor| monitor.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .ok_or_else(|| AleError::Other(anyhow::anyhow!("No monitors found")))?;

    let desktop_x = monitor
        .x()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to read monitor x: {}", e)))?;
    let desktop_y = monitor
        .y()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to read monitor y: {}", e)))?;
    let desktop_width = monitor
        .width()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to read monitor width: {}", e)))?;
    let desktop_height = monitor
        .height()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to read monitor height: {}", e)))?;
    let scale_factor = monitor.scale_factor().unwrap_or(1.0);

    let image = monitor
        .capture_image()
        .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to capture screen: {}", e)))?;

    let width = image.width();
    let height = image.height();

    // 缩放
    let (scaled_w, scaled_h, rgba_data) = if scale < 1.0 {
        let new_w = (width as f32 * scale) as u32;
        let new_h = (height as f32 * scale) as u32;
        let resized =
            image::imageops::resize(&image, new_w, new_h, image::imageops::FilterType::Nearest);
        (new_w, new_h, resized.into_raw())
    } else {
        (width, height, image.into_raw())
    };

    if CAPTURE_VISIBILITY_GENERATION.load(Ordering::Acquire) != capture_generation {
        return Err(AleError::ConfigError(
            "屏幕捕获期间敏感界面状态发生变化".to_string(),
        ));
    }

    Ok(ScreenFrame {
        width: scaled_w,
        height: scaled_h,
        source_width: width,
        source_height: height,
        desktop_x,
        desktop_y,
        desktop_width,
        desktop_height,
        scale_factor,
        rgba_data,
        timestamp: Instant::now(),
        capture_generation,
    })
}

fn capture_is_suspended() -> bool {
    generation_is_suspended(CAPTURE_VISIBILITY_GENERATION.load(Ordering::Acquire))
}

fn generation_is_suspended(generation: u64) -> bool {
    generation & 1 == 1
}

fn set_capture_suspended(suspended: bool) {
    let mut current = CAPTURE_VISIBILITY_GENERATION.load(Ordering::Acquire);
    loop {
        if generation_is_suspended(current) == suspended {
            return;
        }
        match CAPTURE_VISIBILITY_GENERATION.compare_exchange_weak(
            current,
            current.wrapping_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

/// 将帧转换为 JPEG 字节
fn frame_to_jpeg(frame: &ScreenFrame, quality: u8) -> Result<Vec<u8>> {
    let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba_data.clone())
        .ok_or_else(|| AleError::Other(anyhow::anyhow!("Failed to create image from frame")))?;

    let rgb_img = image::DynamicImage::ImageRgba8(img).to_rgb8();

    let mut buf = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    rgb_img
        .write_with_encoder(encoder)
        .map_err(|e| AleError::Other(anyhow::anyhow!("JPEG encode failed: {}", e)))?;

    Ok(buf.into_inner())
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_config_default() {
        let config = CaptureConfig::default();
        assert_eq!(config.interval, Duration::from_secs(3));
        assert_eq!(config.scale, 0.5);
        assert_eq!(config.jpeg_quality, 80);
    }

    fn coordinate_space(
        desktop_x: i32,
        desktop_y: i32,
        scale_factor: f32,
    ) -> ScreenCoordinateSpace {
        ScreenCoordinateSpace {
            image_width: 960,
            image_height: 540,
            source_width: (1920.0 * scale_factor) as u32,
            source_height: (1080.0 * scale_factor) as u32,
            desktop_x,
            desktop_y,
            desktop_width: 1920,
            desktop_height: 1080,
            scale_factor,
        }
    }

    #[test]
    fn maps_corners_and_center_at_common_dpi_scales() {
        for dpi in [1.0, 1.5, 2.0] {
            let space = coordinate_space(0, 0, dpi);
            assert_eq!(space.map_point(0.0, 0.0).unwrap(), (0.0, 0.0));
            assert_eq!(space.map_point(959.0, 539.0).unwrap(), (1919.0, 1079.0));
            let (x, y) = space.map_point(479.5, 269.5).unwrap();
            assert!((x - 959.5).abs() < 0.001);
            assert!((y - 539.5).abs() < 0.001);
        }
    }

    #[test]
    fn maps_right_and_negative_origin_monitors() {
        let right = coordinate_space(1920, 0, 1.0);
        assert_eq!(right.map_point(0.0, 0.0).unwrap(), (1920.0, 0.0));
        assert_eq!(right.map_point(959.0, 539.0).unwrap(), (3839.0, 1079.0));

        let left = coordinate_space(-1920, -200, 2.0);
        assert_eq!(left.map_point(0.0, 0.0).unwrap(), (-1920.0, -200.0));
        assert_eq!(left.map_point(959.0, 539.0).unwrap(), (-1.0, 879.0));
    }

    #[test]
    fn rejects_out_of_bounds_model_coordinates() {
        let space = coordinate_space(0, 0, 1.0);
        for point in [(-1.0, 0.0), (0.0, -1.0), (960.0, 0.0), (0.0, 540.0)] {
            assert!(space.map_point(point.0, point.1).is_err());
        }
    }

    #[test]
    fn suspension_invalidates_cached_frames_across_capture_instances() {
        set_capture_suspended(false);
        let generation = CAPTURE_VISIBILITY_GENERATION.load(Ordering::Acquire);
        let first = ScreenCapture::new(CaptureConfig::default());
        let second = ScreenCapture::new(CaptureConfig::default());
        let frame = ScreenFrame {
            width: 1,
            height: 1,
            source_width: 1,
            source_height: 1,
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: 1,
            desktop_height: 1,
            scale_factor: 1.0,
            rgba_data: vec![0; 4],
            timestamp: Instant::now(),
            capture_generation: generation,
        };
        *first.latest_frame.lock().unwrap() = Some(frame.clone());
        *second.latest_frame.lock().unwrap() = Some(frame);
        assert!(first.latest_frame().is_some());
        assert!(second.latest_frame().is_some());

        first.set_suspended(true);
        assert!(first.latest_frame().is_none());
        assert!(second.latest_frame().is_none());

        first.set_suspended(false);
        assert!(first.latest_frame().is_none());
        assert!(second.latest_frame().is_none());
    }

    #[test]
    #[ignore = "requires screen-capture permission and a real display"]
    fn captures_real_monitor_coordinate_metadata() {
        let frame = capture_primary_monitor(0.5).unwrap();
        let space = frame.coordinate_space();
        assert!(space.image_width > 0 && space.image_height > 0);
        assert!(space.desktop_width > 0 && space.desktop_height > 0);
        assert!(space.scale_factor > 0.0);
        assert!(space.map_point(0.0, 0.0).is_ok());
    }
}
