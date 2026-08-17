# Ale, My Eyes! Code Wiki (Archived)

> [!WARNING]
> 本文档是桌面与移动仓库拆分前的历史快照，不代表当前支持矩阵、构建方式或发布流程。当前桌面端仅支持 Windows 与 Linux/NixOS；Android 由独立移动仓库维护；macOS 与 iOS 不再支持。请以根目录 `README.md`、`AGENTS.md` 和 `docs/PC-ISSUES-STATUS.md` 为准，不要使用下文中的旧打包命令或产物名称。

---

## 目录

1. [项目概述](#1-项目概述)
2. [项目结构](#2-项目结构)
3. [工作空间与依赖管理](#3-工作空间与依赖管理)
4. [核心库 ale-core](#4-核心库-ale-core)
   - [4.1 lib.rs — AleEngine 主入口](#41-librs--aleengine-主入口)
   - [4.2 config.rs — 配置系统](#42-configrs--配置系统)
   - [4.3 cloud.rs — 云端 API](#43-cloudrs--云端-api)
   - [4.4 inference.rs — 自适应推理引擎](#44-inferencers--自适应推理引擎)
   - [4.5 vad.rs — 语音活动检测](#45-vadrs--语音活动检测)
   - [4.6 actions.rs — 操作指令协议](#46-actionsrs--操作指令协议)
   - [4.7 context.rs — 上下文与记忆管理](#47-contextrs--上下文与记忆管理)
   - [4.8 memory.rs — 长期记忆持久化](#48-memoryrs--长期记忆持久化)
   - [4.9 manager.rs — 智能模型管理器](#49-managerrs--智能模型管理器)
   - [4.10 downloader.rs — 模型下载器](#410-downloaderrs--模型下载器)
   - [4.11 条件编译模块](#411-条件编译模块)
   - [4.12 types.rs & error.rs](#412-typesrs--errorrs)
5. [命令行工具 ale-cli](#5-命令行工具-ale-cli)
6. [桌面/Android 应用 ale-gui](#6-桌面android-应用-ale-gui)
   - [6.1 架构概述](#61-架构概述)
   - [6.2 lib.rs — 应用主逻辑](#62-librs--应用主逻辑)
   - [6.3 UI 层 (Slint)](#63-ui-层-slint)
   - [6.4 audio.rs — 录音模块](#64-audiors--录音模块)
   - [6.5 conversation.rs — 对话处理](#65-conversationrs--对话处理)
   - [6.6 screen_capture.rs — 屏幕捕获](#66-screen_capturers--屏幕捕获)
   - [6.7 automation.rs — 桌面自动化](#67-automationrs--桌面自动化)
   - [6.8 camera.rs — Android 相机](#68-cameras--android-相机)
   - [6.9 tts_player.rs — 语音播放](#69-tts_playerrs--语音播放)
   - [6.10 file_picker.rs — 文件选择](#610-file_pickerrs--文件选择)
   - [6.11 android.rs — Android 入口](#611-androidrs--android-入口)
7. [Feature Flags 机制](#7-feature-flags-机制)
8. [数据流架构](#8-数据流架构)
9. [依赖关系图](#9-依赖关系图)
10. [构建与运行](#10-构建与运行)
11. [CI/CD 流程](#11-cicd-流程)
12. [测试体系](#12-测试体系)
13. [配置与数据目录](#13-配置与数据目录)

---

## 1. 项目概述

**Ale, My Eyes!** 是一个基于 Rust 的跨平台智能视觉辅助系统。用户对着屏幕或摄像头说话，AI 用自然语言回答问题，并可在桌面端自动执行键鼠操作。

**核心特性：**
- **语音交互**：启动即监听，VAD 自动检测说话结束，支持 17+ 种语言
- **视觉问答**：对屏幕/摄像头提问，AI 结合画面自然语言回答
- **桌面自动化**：通过 Function Calling 返回结构化操作计划，用户确认后自动执行
- **自适应推理**：根据设备性能和网络状态自动选择本地/云端推理
- **长期记忆**：自动从对话中提取用户偏好并持久化

**历史快照所描述的平台：** Windows、Linux 与已拆分的 Android 实现；这不是当前支持声明。

---

## 2. 项目结构

```
ale-my-eyes-master/
├── Cargo.toml                    # Workspace 根配置
├── Cargo.lock                    # 依赖锁定文件
├── AGENTS.md                     # AI Agent 开发规范
├── README.md                     # 项目说明
├── LICENSE                       # MIT 许可证
│
├── ale-core/                     # 核心库 (rlib + cdylib)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                # AleEngine 主入口 + 模块导出
│       ├── config.rs             # 配置系统 (AppConfig, ConfigManager)
│       ├── cloud.rs              # 云端 API (OpenAI 兼容)
│       ├── inference.rs          # 自适应推理引擎
│       ├── vad.rs                # 语音活动检测 (VAD)
│       ├── actions.rs            # 操作指令协议 (11 种操作类型)
│       ├── context.rs            # 上下文管理 (对话/视觉/记忆)
│       ├── memory.rs             # 长期记忆持久化 (文件式 .md 存储)
│       ├── manager.rs            # 智能模型管理器
│       ├── downloader.rs         # 模型下载器 (HuggingFace)
│       ├── types.rs              # 公共类型定义
│       ├── error.rs              # 错误类型 (AleError)
│       ├── asr.rs                # 本地 ASR [feature: local-inference]
│       ├── vlm.rs                # 本地 VLM [feature: local-inference]
│       ├── llm.rs                # 本地 LLM [feature: local-inference]
│       └── tts.rs                # 系统 TTS [feature: tts]
│
├── ale-cli/                      # 命令行工具
│   ├── Cargo.toml
│   └── src/
│       └── main.rs               # CLI 入口 (clap)
│
├── ale-gui/                      # 跨平台 GUI (desktop + Android)
│   ├── Cargo.toml
│   ├── build.rs                  # Slint 编译脚本
│   ├── ui/
│   │   ├── app.slint             # 主窗口定义 (360x190px)
│   │   ├── compact.slint         # 主界面布局
│   │   ├── settings-popup.slint  # 设置弹窗
│   │   └── widgets.slint         # 通用 UI 组件
│   └── src/
│       ├── lib.rs                # AppState + setup_app() + 共享逻辑
│       ├── main.rs               # Desktop 入口
│       ├── android.rs            # Android 入口 (cdylib)
│       ├── audio.rs              # 录音 (cpal/oboe)
│       ├── conversation.rs       # 对话处理 + TTS + Function Calling
│       ├── screen_capture.rs     # 屏幕截图 (xcap)
│       ├── automation.rs         # 键鼠自动化 (enigo)
│       ├── camera.rs             # Android 相机 (JNI Camera2)
│       ├── tts_player.rs         # 音频播放 (rodio/JNI MediaPlayer)
│       └── file_picker.rs        # 文件选择 (rfd/JNI)
│
├── docs/
│   ├── API.md                    # 历史 HTTP API 文档
│   └── CODE-WIKI.md              # 本技术文档
│
├── scripts/
│   ├── package-linux.sh          # Linux 打包脚本
│   ├── package-windows.sh        # Windows 打包脚本
│   ├── package-android.sh        # Android 打包脚本
│   ├── create-release.sh         # 源码发布脚本
│   └── build-release.sh          # 通用构建脚本
│
└── .github/workflows/
    ├── build.yml                 # 主 CI/CD (check + Windows + Linux + Android + Release)
    └── android.yml               # 独立 Android 构建
```

---

## 3. 工作空间与依赖管理

### 3.1 Workspace 配置

根 `Cargo.toml` 定义了 Cargo Workspace，包含三个 crate：

```toml
[workspace]
members = ["ale-core", "ale-cli", "ale-gui"]
resolver = "2"
```

### 3.2 Workspace 公共依赖

通过 `[workspace.dependencies]` 统一版本管理：

| 依赖 | 版本 | 用途 |
|------|------|------|
| `tokio` | 1 (full) | 异步运行时 |
| `serde` / `serde_json` | 1 | 序列化/反序列化 |
| `anyhow` / `thiserror` | 1 | 错误处理 |
| `tracing` | 0.1 | 结构化日志 |
| `bytes` | 1 | 字节缓冲区 |
| `image` | 0.25 | 图像处理 |
| `chrono` | 0.4 (serde) | 日期时间 |
| `uuid` | 1 (v4, serde) | 唯一标识符 |
| `base64` | 0.22 | Base64 编解码 |
| `async-trait` | 0.1 | 异步 trait |
| `walkdir` | 2 | 目录遍历 |

### 3.3 Crate 间依赖关系

```
ale-cli  ──→  ale-core
ale-gui  ──→  ale-core
```

- `ale-core`：无内部依赖，是纯库 crate
- `ale-cli`：依赖 `ale-core` + `clap`（命令行解析）+ `tracing-subscriber`（日志）
- `ale-gui`：依赖 `ale-core` + Slint（UI）+ 平台特定库

### 3.4 ale-core 独有依赖

| 依赖 | 用途 | 条件 |
|------|------|------|
| `reqwest` (rustls-tls, multipart, stream) | HTTP 客户端 | 始终 |
| `url` | URL 解析 | 始终 |
| `dirs` | 系统目录路径 | 始终 |
| `whisper-rs` | 本地 ASR | `local-inference` |
| `ort` (load-dynamic) | ONNX Runtime | `local-inference` |
| `ndarray` | 数组计算 | `local-inference` |
| `tts` | 系统 TTS | `tts` |

### 3.5 ale-gui 平台特定依赖

**Desktop（非 Android）：**

| 依赖 | 用途 |
|------|------|
| `slint` 1.16 | 跨平台 UI 框架 |
| `cpal` 0.15 | 跨平台音频输入 |
| `rodio` 0.19 | 音频播放 |
| `xcap` 0.9 | 屏幕截图 |
| `enigo` 0.6 | 键鼠自动化 |
| `rfd` 0.15 | 原生文件对话框 |
| `open` 5 | 打开 URL/文件 |

**Android：**

| 依赖 | 用途 |
|------|------|
| `slint` (backend-android-activity-06) | Android UI 后端 |
| `oboe` 0.6 | 低延迟音频 |
| `jni` 0.21 | Java Native Interface |
| `ndk-context` 0.1 | Android 上下文 |

---

## 4. 核心库 ale-core

### 4.1 lib.rs — AleEngine 主入口

**文件：** `ale-core/src/lib.rs`

`AleEngine` 是整个应用的核心引擎，整合了所有功能子系统。

#### 关键结构体

```rust
pub struct AleEngine {
    config_manager: ConfigManager,
    model_manager: Arc<Mutex<SmartModelManager>>,
    inference_engine: AdaptiveInference,
    cloud_api: bool,                         // 云端 API 是否就绪
    context_manager: ContextManager,
    memory_store: MemoryStore,
    #[cfg(feature = "tts")]
    tts: Option<Box<dyn TextToSpeech>>,
}
```

#### 关键方法

| 方法 | 签名 | 说明 |
|------|------|------|
| `new` | `async fn new(config_path: &Path) -> Result<Self>` | 创建引擎：加载配置、检测设备/网络、创建模型管理器、初始化推理引擎、加载 ASR 模型（如启用）、加载长期记忆 |
| `transcribe` | `async fn transcribe(&self, audio_data: &[u8]) -> Result<String>` | 语音识别（通过推理引擎路由） |
| `synthesize` | `async fn synthesize(&self, text: &str) -> Result<Vec<u8>>` | 语音合成（优先本地 TTS，降级到云端） |
| `describe_image` | `async fn describe_image(&self, image_data: &[u8]) -> Result<String>` | 图像描述 |
| `ask_about_image` | `async fn ask_about_image(&self, image_data: &[u8], question: &str) -> Result<VisionResponse>` | 视觉问答（带上下文） |
| `ask_about_image_with_tools` | `async fn ask_about_image_with_tools(&self, image_data: &[u8], question: &str, tools: Vec<Value>) -> Result<VisionResponse>` | 视觉问答 + Function Calling |
| `ask_text` | `async fn ask_text(&self, question: &str) -> Result<CloudResponse>` | 纯文本问答（无图像） |
| `learn_from_interaction` | `fn learn_from_interaction(&mut self, question: &str, answer: &str) -> Result<usize>` | 从交互中自动提取并持久化长期记忆 |
| `add_memory` / `delete_memory` / `clear_memories` | — | 长期记忆 CRUD |
| `update_config` | `fn update_config(&mut self, config: AppConfig) -> Result<()>` | 更新配置并持久化 |
| `status` | `async fn status(&self) -> EngineStatus` | 获取引擎状态 |

#### AleEngineFactory

```rust
pub struct AleEngineFactory;
impl AleEngineFactory {
    pub async fn create_default() -> Result<AleEngine>;       // 使用默认配置路径
    pub async fn create_with_config(path: &Path) -> Result<AleEngine>;
    pub async fn create_test() -> Result<AleEngine>;           // 使用 /tmp 测试路径
}
```

#### 模块导出

```rust
pub mod actions;
pub mod cloud;
pub mod config;
pub mod context;
pub mod downloader;
pub mod error;
pub mod inference;
pub mod manager;
pub mod memory;
pub mod types;
pub mod vad;

#[cfg(feature = "tts")]       pub mod tts;
#[cfg(feature = "local-inference")] pub mod asr;
#[cfg(feature = "local-inference")] pub mod vlm;
#[cfg(feature = "local-inference")] pub mod llm;
```

---

### 4.2 config.rs — 配置系统

**文件：** `ale-core/src/config.rs`

#### 配置结构体层次

```
AppConfig
├── cloud_api: CloudApiConfig     # 云端 API 配置
│   ├── provider: String          # "openai" / "anthropic" / "google" / "azure" / 自定义
│   ├── api_key: String
│   ├── api_url: String           # 默认 "https://api.openai.com/v1"
│   ├── model: String             # 默认 "gpt-4o"
│   ├── max_tokens: usize         # 默认 1024
│   └── timeout: u32              # 秒，默认 30
│
├── models: ModelsConfig           # 模型管理配置
│   ├── auto_download: bool       # 默认 true
│   ├── max_download_size: u64    # 字节，默认 500MB
│   ├── preferred_quality: String # "low" / "balanced" / "high"
│   ├── offline_mode: bool
│   └── models_dir: String        # 默认 "models"
│
├── inference: InferenceConfig     # 推理配置
│   ├── mode: String              # "local" / "cloud" / "adaptive"
│   ├── prefer_cloud: bool        # 默认 true
│   ├── timeout: u32              # 秒，默认 30
│   └── fallback_to_local: bool   # 默认 true
│
├── audio: AudioConfig             # 音频配置
│   ├── sample_rate: u32          # 默认 16000
│   ├── channels: u16             # 默认 1
│   ├── buffer_size: u32          # 默认 4096
│   ├── voice: String             # 默认 "default"
│   └── speed: f32                # 默认 1.0
│
└── ui: UiConfig                   # 界面配置
    ├── language: String           # 默认 "zh-CN"
    ├── theme: String              # 默认 "system"
    ├── font_size: u32             # 默认 16
    ├── high_contrast: bool
    ├── screen_reader: bool        # 默认 true
    └── auto_speak: bool           # 默认 true，AI 回答自动朗读
```

所有配置结构体均使用 `#[serde(default)]` 标注，允许 JSON 中缺失部分字段。

#### 关键类型

| 类型 | 说明 |
|------|------|
| `ConfigManager` | 配置管理器：加载、保存、更新、验证配置 |
| `ConfigFactory` | 工厂：`create_default()`（系统配置目录）、`create_test()`（`/tmp`） |
| `ConfigMigrator` | 版本迁移器：支持 v1.0 → v2.0 配置格式迁移 |
| `ConfigValidator` | 验证器：校验 API Key、URL 格式、推理模式合法性等 |

#### 配置文件路径

- 默认：`~/.config/ale-my-eyes/config.json`（通过 `dirs::config_dir()` 获取）
- 测试：`/tmp/ale-my-eyes-test/config.json`

---

### 4.3 cloud.rs — 云端 API

**文件：** `ale-core/src/cloud.rs`

#### 核心 Trait

```rust
#[async_trait]
pub trait CloudApi: Send + Sync {
    async fn chat(&self, messages: Vec<CloudMessage>) -> Result<CloudResponse>;
    async fn vision(&self, image_data: &[u8], prompt: &str) -> Result<CloudResponse>;
    async fn vision_ask(&self, image_data: &[u8], question: &str, tools: Option<Vec<Value>>) -> Result<VisionResponse>;
    async fn transcribe(&self, audio_data: &[u8]) -> Result<CloudResponse>;
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>>;
    async fn health_check(&self) -> Result<bool>;
}
```

#### 关键类型

| 类型 | 说明 |
|------|------|
| `CloudProvider` | 枚举：`OpenAI` / `Anthropic` / `Google` / `Azure` / `Custom(String)` |
| `CloudConfig` | 运行时配置：provider, api_key, api_url, model, max_tokens, timeout, retry_count |
| `CloudMessage` | 消息：role + content |
| `CloudResponse` | 响应：content, tokens_used, model, provider |
| `VisionResponse` | 视觉响应：content, tool_calls, tokens_used, model |
| `ToolCall` | 工具调用：id + FunctionCall |
| `FunctionCall` | 函数调用：name + arguments (JSON string) |

#### OpenAIApi 实现

当前仅实现了 `OpenAIApi`（兼容 OpenAI API 格式），其他 provider 回退到同一实现。

**API 端点映射：**
- `chat()` → `POST /chat/completions`（模型 `gpt-4o`）
- `vision()` → `POST /chat/completions`（带 `image_url` content）
- `vision_ask()` → `POST /chat/completions`（带 image + tools 定义）
- `transcribe()` → `POST /audio/transcriptions`（multipart form，模型 `whisper-1`）
- `synthesize()` → `POST /audio/speech`（模型 `tts-1`，voice `alloy`）
- `health_check()` → `GET /models`

#### CloudApiFactory

```rust
pub struct CloudApiFactory;
impl CloudApiFactory {
    pub fn create(config: CloudConfig) -> Box<dyn CloudApi>;
    // OpenAI → OpenAIApi，其他 provider → 回退到 OpenAIApi
}
```

---

### 4.4 inference.rs — 自适应推理引擎

**文件：** `ale-core/src/inference.rs`

根据设备性能和网络状态自动选择推理后端（本地/云端）。

#### 核心枚举

| 枚举 | 变体 | 说明 |
|------|------|------|
| `DevicePerformance` | `Low` / `Medium` / `High` | 设备性能等级 |
| `NetworkStatus` | `Offline` / `Weak` / `Normal` / `Fast` | 网络状态 |
| `InferenceMode` | `LocalOnly` / `CloudOnly` / `Adaptive` | 推理模式 |
| `TaskComplexity` | `Simple` / `Medium` / `Complex` | 任务复杂度 |

#### AdaptiveInference

```rust
pub struct AdaptiveInference {
    config: InferenceConfig,
    cloud_api: Option<Box<dyn CloudApi>>,
    #[cfg(feature = "local-inference")]
    local_asr: Option<WhisperRecognizer>,
}
```

**关键方法：**

| 方法 | 说明 |
|------|------|
| `detect_device_performance()` | 检测设备性能（当前返回默认 Medium） |
| `detect_network_status()` | 检测网络状态（当前返回默认 Normal） |
| `select_inference_mode(complexity)` | 根据任务复杂度、网络、配置选择推理模式 |
| `transcribe(audio_data)` | 语音识别路由 |
| `describe_image(image_data)` | 图像描述路由 |
| `ask_about_image(image_data, question, tools)` | 视觉问答路由 |
| `generate(prompt)` | 文本生成路由 |

**推理模式选择逻辑：**
- `LocalOnly` / `CloudOnly`：强制使用指定后端
- `Adaptive`：
  - 简单任务 → 优先云端（低延迟）
  - 中等任务 → 根据网络状态决定
  - 复杂任务 → 优先云端（高质量），离线降级本地

#### InferenceResult

```rust
pub struct InferenceResult<T> {
    pub data: T,
    pub mode_used: InferenceMode,
    pub latency: Duration,
    pub tokens_used: Option<usize>,
}
```

---

### 4.5 vad.rs — 语音活动检测

**文件：** `ale-core/src/vad.rs`

基于能量的简易 VAD 实现，使用状态机模式。

#### 状态机

```
Silent ──(连续 speech_start_frames 帧能量 > 阈值)──→ Speaking
Speaking ──(连续 silence_end_frames 帧能量 < 阈值)──→ SpeechEnded
SpeechEnded ──(下一帧)──→ Silent
```

#### 关键类型

| 类型 | 说明 |
|------|------|
| `VadState` | `Silent` / `Speaking` / `SpeechEnded` |
| `VadConfig` | 配置：energy_threshold (0.02), speech_start_frames (3), silence_end_frames (15), sample_rate (16000), frame_size (320) |
| `VoiceActivityDetector` | VAD 检测器，维护状态、帧计数、能量历史 |

#### 关键方法

| 方法 | 说明 |
|------|------|
| `process_frame(samples: &[f32]) -> VadState` | 处理一帧音频，返回当前状态 |
| `adapt_threshold()` | 基于历史能量自适应调整阈值 |
| `average_energy()` | 获取平均能量 |

#### 工具函数

| 函数 | 说明 |
|------|------|
| `process_audio_chunks(vad, audio_data)` | 将音频按帧分割并处理 |
| `i16_to_f32(data)` | i16 PCM → f32 |
| `pcm16_bytes_to_f32(data)` | 字节 PCM16 → f32 |

---

### 4.6 actions.rs — 操作指令协议

**文件：** `ale-core/src/actions.rs`

定义了 11 种桌面自动化操作类型和 3 级风险评估。

#### Action 枚举

| 操作 | 字段 | 风险等级 |
|------|------|----------|
| `Click` | x, y, button | Medium |
| `DoubleClick` | x, y | Medium |
| `MouseMove` | x, y | Low |
| `Scroll` | x, y, delta_x, delta_y | Low |
| `Type` | text | Medium |
| `Key` | key, modifiers | Medium |
| `Wait` | ms | Low |
| `OpenApp` | name | Medium |
| `CloseApp` | name | High |
| `OpenUrl` | url | Medium |
| `FileOperation` | operation, path, target | High |

#### RiskLevel

```
Low (滚动/移动/等待) < Medium (点击/打字/快捷键/打开) < High (关闭/文件操作)
```

#### ActionPlan

```rust
pub struct ActionPlan {
    pub actions: Vec<Action>,
    pub risk_level: RiskLevel,
    pub explanation: String,
    pub requires_confirmation: bool,  // risk_level >= High 时自动设为 true
}
```

#### 关键函数

| 函数 | 说明 |
|------|------|
| `parse_action_plan(json)` | 从 JSON 解析 ActionPlan |
| `parse_action_plan_arguments(json)` | 从 Function Call arguments 解析，支持直接或 `{ "plan": ... }` 包装 |

---

### 4.7 context.rs — 上下文与记忆管理

**文件：** `ale-core/src/context.rs`

管理对话历史、视觉记忆和长期记忆，构建发送给 AI 的消息列表。

#### ContextManager

```rust
pub struct ContextManager {
    conversation: VecDeque<ContextEntry>,    // 对话历史
    visual_memory: VecDeque<FrameSummary>,   // 视觉记忆（最近 5 帧）
    long_term_memory: Vec<MemoryEntry>,      // 长期记忆
    conversation_summary: Option<String>,    // 压缩后的旧对话摘要
    current_tokens: usize,                   // 当前 token 估算
    max_tokens: usize,                       // token 预算
    session_tokens_used: usize,              // 会话累计 token
    system_prompt: String,
}
```

#### 关键方法

| 方法 | 说明 |
|------|------|
| `add_user_message(content)` | 添加用户消息 |
| `add_assistant_message(content)` | 添加助手消息 |
| `add_frame_summary(summary)` | 添加视觉帧摘要（自动去重） |
| `add_memory(entry)` | 添加长期记忆（自动去重） |
| `build_messages(image_desc, question)` | 构建发送给 AI 的消息列表 |
| `relevant_memories(query, limit)` | 基于查询选择最相关的长期记忆 |
| `maybe_compact()` | token 达到 70% 时自动压缩旧对话 |

#### build_messages 消息结构

```
1. System prompt（含相关长期记忆）
2. 视觉上下文（最近 2 帧摘要）
3. 对话摘要（如有）
4. 最近 10 条对话
5. 当前问题 + 当前图像描述
```

#### MemoryEntry

```rust
pub struct MemoryEntry {
    pub id: String,
    pub name: String,              // 简短标题
    pub description: String,       // 一行描述
    pub content: String,           // 记忆内容
    pub memory_type: MemoryType,   // User / Feedback / Project / Reference
    pub importance: f32,           // 0.0-1.0
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
}
```

#### MemoryType

借鉴 Claude Code Memory System 的四类闭合分类法：
- `User`：用户画像（角色、偏好、知识水平）
- `Feedback`：行为反馈（纠正、确认、风格指导）
- `Project`：项目上下文（目标、进度、决策）
- `Reference`：外部引用

#### Token 估算

- 1 中文字符 ≈ 2 tokens
- 1 英文字符 ≈ 0.25 tokens

---

### 4.8 memory.rs — 长期记忆持久化

**文件：** `ale-core/src/memory.rs`

文件式持久化存储，每条记忆保存为独立 `.md` 文件（含 YAML frontmatter 元数据）。

#### MemoryStore

```rust
pub struct MemoryStore {
    dir_path: PathBuf,      // 记忆目录
    index_path: PathBuf,    // MEMORY.md 索引
    memories: Vec<MemoryEntry>,
}
```

#### 存储格式

每条记忆保存为 `{type}_{slug}.md`：

```markdown
---
id: uuid
name: 偏好简洁回答
description: 用户喜欢简洁的回答方式
type: feedback
importance: 0.85
created_at: 2024-01-01T00:00:00Z
tags: [偏好, 表达]
---

用户喜欢简洁的回答
```

`MEMORY.md` 作为轻量索引文件，格式为 Markdown 列表。

#### 关键方法

| 方法 | 说明 |
|------|------|
| `load_or_create(dir_path)` | 加载或创建记忆存储 |
| `add(entry)` | 添加记忆（写 .md 文件 + 更新索引） |
| `delete(id)` | 删除记忆（删 .md 文件 + 更新索引） |
| `clear()` | 清空所有记忆 |
| `search(query, limit)` | 基于 term-matching 评分搜索 |

#### 自动记忆提取

`extract_memories(question, answer)` 函数从交互中自动提取候选记忆：

1. **显式偏好**：检测"简洁"、"详细"、"中文"、"英文"、"语速"等关键词
2. **环境记忆**：检测浏览器（Firefox/Chrome）、操作系统（Windows/macOS/Linux/Android）
3. **无障碍需求**：检测"无障碍"、"屏幕阅读器"等关键词
4. **显式记忆请求**：检测"记住"、"以后"、"下次"、"保存"等标记
5. **软信号**：当无显式信号时，从"需要辅助"等弱信号中提取

#### 旧版迁移

自动检测并迁移旧版 `memory.json` → 文件式存储（重命名旧文件为 `.json.bak`）。

---

### 4.9 manager.rs — 智能模型管理器

**文件：** `ale-core/src/manager.rs`

根据设备性能和网络状态智能管理本地模型的下载、选择和使用。

#### SmartModelManager

```rust
pub struct SmartModelManager {
    downloader: ModelDownloader,
    config: ModelConfig,
    model_status: HashMap<String, ModelStatus>,
    device_performance: DevicePerformance,
    network_status: NetworkStatus,
    cloud_api: Option<Box<dyn CloudApi>>,
}
```

#### ModelStrategy

| 策略 | 说明 |
|------|------|
| `LocalOnly` | 仅使用本地模型 |
| `CloudOnly` | 仅使用云端 |
| `Smart` | 根据网络和设备智能选择 |
| `Custom(Vec<String>)` | 用户自定义模型列表 |

#### 智能选择逻辑（Smart 策略）

1. 离线模式 → 本地模型
2. 离线状态 → 本地模型
3. 弱网 → 优先本地，降级云端
4. 正常/高速网络：
   - 简单任务（ASR/TTS）→ 优先本地（低延迟）
   - 复杂任务（VLM/LLM）→ 优先云端（高质量）

#### ModelManagerFactory

```rust
pub struct ModelManagerFactory;
impl ModelManagerFactory {
    pub fn create_default(models_dir: &Path) -> SmartModelManager;
    pub fn create_for_device(models_dir, performance, network) -> SmartModelManager;
    pub fn create_offline(models_dir: &Path) -> SmartModelManager;
}
```

---

### 4.10 downloader.rs — 模型下载器

**文件：** `ale-core/src/downloader.rs`

从 HuggingFace 下载模型文件，支持进度回调和并发控制。

#### 内置模型列表

| ID | 名称 | 大小 | 用途 | 推荐设备 |
|----|------|------|------|----------|
| `whisper-tiny` | Whisper Tiny | 75MB | 基础语音识别 | 低端设备 |
| `whisper-small` | Whisper Small | 244MB | 高质量语音识别 | 中端设备 |
| `whisper-large-v3` | Whisper Large V3 | 1.5GB | 专业级语音识别 | 高端设备 |
| `piper-zh_CN` | Piper 中文语音 | 50MB | 中文语音合成 | 所有设备 |
| `piper-en_US` | Piper 英文语音 | 50MB | 英文语音合成 | 所有设备 |

#### 关键类型

| 类型 | 说明 |
|------|------|
| `ModelInfo` | 模型元数据：id, name, description, size, repo, filename, quantization, purpose, recommended_for |
| `DownloadProgress` | 下载进度：model_id, total_bytes, downloaded_bytes, progress, speed, eta |
| `ModelDownloader` | 下载器：管理已知模型列表、下载/删除/查询 |
| `ModelDownloadManager` | 下载管理器：支持并发下载和进度回调 |

#### 下载流程

1. 检查模型是否已下载（文件存在）
2. 构建 HuggingFace URL：`https://huggingface.co/{repo}/resolve/main/{filename}`
3. 流式下载到 `.tmp` 临时文件
4. 下载完成后重命名为最终文件名

---

### 4.11 条件编译模块

#### asr.rs — 本地语音识别 [feature: local-inference]

基于 `whisper-rs`（whisper.cpp FFI）的本地 ASR 实现。

```rust
pub struct WhisperRecognizer {
    model_path: PathBuf,
    ctx: Option<WhisperContext>,
    language: Option<String>,
    n_threads: i32,
}
```

**关键特性：**
- 支持 WAV 和 raw PCM16 音频输入
- 自动立体声→单声道转换
- 自动重采样到 16kHz
- 支持 17 种语言（auto/en/zh/ja/ko/fr/de/es/ru/pt/it/nl/pl/ar/tr/vi/th）
- 自动检测 CPU 核心数设置线程数

#### vlm.rs — 本地视觉语言模型 [feature: local-inference]

基于 ONNX Runtime 的 VLM 框架（当前 `describe_image` 未实现）。

```rust
pub struct OnnxVlm {
    model_path: PathBuf,
    session: Option<ort::Session>,
}
```

#### llm.rs — 本地大语言模型 [feature: local-inference]

支持本地和远程两种后端：

```rust
pub enum LlmBackend { Local, Remote, Onnx, Candle }

pub struct LocalLlm { config: LlmConfig, model: Option<LlamaModel> }
pub struct RemoteLlm { config: LlmConfig, client: reqwest::Client }
```

#### tts.rs — 系统 TTS [feature: tts]

基于 `tts` crate 的系统 TTS 引擎（当前 `synthesize` 未实现）。

```rust
pub struct SystemTts {
    voice: Option<String>,
    tts_engine: Option<tts::Tts>,
}
```

---

### 4.12 types.rs & error.rs

**文件：** `ale-core/src/types.rs`、`ale-core/src/error.rs`

#### 公共类型

```rust
pub struct EngineStatus { pub cloud_ready: bool, pub tts_ready: bool }
pub struct ImageDescriptionRequest/Response { ... }
pub struct TranscriptionRequest/Response { ... }
pub struct SynthesisRequest/Response { ... }
pub struct ModelInfo { pub name, version, device, loaded }
pub struct SystemStatus { pub engine, models, platform, version }
```

#### 错误类型

```rust
#[derive(Error, Debug)]
pub enum AleError {
    AsrError(String),
    TtsError(String),
    VlmError(String),
    CloudApiError(String),
    ConfigError(String),
    NotInitialized(&'static str),
    IoError(std::io::Error),
    ImageError(image::ImageError),
    SerializationError(serde_json::Error),
    Other(anyhow::Error),
}
```

---

## 5. 命令行工具 ale-cli

**文件：** `ale-cli/src/main.rs`

基于 `clap` 的命令行工具，提供五个子命令。

### 子命令

| 命令 | 参数 | 说明 |
|------|------|------|
| `transcribe` | `--audio <file>` [--output <file>] | 语音识别 |
| `synthesize` | `--text <text>` --output <file> [--voice <voice>] | 语音合成 |
| `describe` | `--image <file>` [--output <file>] | 图像描述 |
| `test-connection` | — | 测试云端连接 |
| `status` | — | 显示引擎状态 |

### 使用示例

```bash
cargo run -p ale-cli -- transcribe --audio input.wav
cargo run -p ale-cli -- describe --image screenshot.png
cargo run -p ale-cli -- test-connection
cargo run -p ale-cli -- status
```

---

## 6. 桌面/Android 应用 ale-gui

### 6.1 架构概述

`ale-gui` 采用 Slint 1.16 作为 UI 框架，使用 `setup_app()` 函数将 Rust 逻辑绑定到 Slint 组件。

**双入口设计：**
- **Desktop**：`src/main.rs` → `AppWindow::new()` + `setup_app()` + `app.run()`
- **Android**：`src/android.rs` → `android_main(app)` → `slint::android::init()` + `AppWindow::new()` + `setup_app()` + `window.run()`

**Crate 类型：** `rlib` + `cdylib`（cdylib 用于 Android 动态库）

### 6.2 lib.rs — 应用主逻辑

**文件：** `ale-gui/src/lib.rs`

#### AppState

```rust
pub struct AppState {
    engine: Option<Arc<Mutex<AleEngine>>>,
    recorder: Option<audio::Recorder>,
    recording_started: Option<Instant>,
    vad_sample_offset: usize,
    auto_speak: bool,
    vad: VoiceActivityDetector,
    vad_active: bool,
    screen_capture: Option<ScreenCapture>,     // Desktop only
    automation: Option<AutomationEngine>,       // Desktop only
    camera: Option<AndroidCamera>,             // Android only
    pending_plan: Option<ActionPlan>,
}
```

#### setup_app() 初始化流程

```
1. 创建 AppState (Arc<Mutex<>>)
2. 异步初始化 AleEngine
   ├── 加载配置 → 更新 UI 设置字段
   ├── 初始化平台服务（屏幕截图/相机、自动化引擎）
   └── 自动开始连续监听
3. 启动 VAD 定时器（每 100ms 检查语音结束）
4. 绑定 UI 回调
   ├── on_text_submitted → 文本问答
   ├── on_confirm_action → 执行操作计划
   ├── on_cancel_action → 取消操作
   ├── on_open_settings / on_close_settings
   ├── on_save_settings → 保存并重建引擎
   ├── on_test_connection → 测试云端连接
   └── 设置字段变更回调
```

#### VAD 处理流程

```
每 100ms:
  1. 获取录音器新增样本 (samples_since)
  2. PCM16 → f32 转换
  3. 按帧送入 VAD 状态机
  4. 若 SpeechEnded:
     a. 停止录音
     b. 转为 WAV
     c. 获取屏幕/相机图像
     d. 调用 engine.transcribe() 语音识别
     e. 调用 handle_question_response() 处理问答
     f. 重启连续监听
```

#### 关键辅助函数

| 函数 | 说明 |
|------|------|
| `spawn_local_task(future)` | 在 Slint 事件循环中异步执行任务 |
| `initialize_platform_services(state)` | 初始化屏幕截图 + 自动化引擎（Desktop）或相机（Android） |
| `start_continuous_listening(state, app)` | 开始录音 + VAD |
| `apply_config_to_app(app, config)` | 将配置同步到 UI |
| `config_from_app(app, base)` | 从 UI 读取配置 |
| `create_engine()` | 创建 AleEngine 实例 |
| `save_settings(engine, config)` | 保存配置并重建引擎 |
| `test_connection(engine)` | 测试云端连接 |

### 6.3 UI 层 (Slint)

**目录：** `ale-gui/ui/`

| 文件 | 说明 |
|------|------|
| `app.slint` | 主窗口定义（360x190px 通知卡片），导入其他 slint 文件 |
| `compact.slint` | 主界面布局：状态指示器、转写文本、AI 回答、token 计数器、输入框 |
| `settings-popup.slint` | 设置弹窗：Provider/API Key/API URL/Model/Max Tokens/Auto Speak |
| `widgets.slint` | 通用组件 + `Palette` 着色系统（自动适配平台风格） |

### 6.4 audio.rs — 录音模块

**文件：** `ale-gui/src/audio.rs`

#### Recorder

```rust
pub struct Recorder {
    stream: cpal::Stream,          // Desktop only
    samples: Arc<Mutex<Vec<f32>>>,
    sample_rate: u32,
    channels: u16,
}
```

**平台实现：**
- **Desktop**：使用 `cpal` 获取默认输入设备，支持 f32/i16/u16 采样格式
- **Android**：使用 `oboe` 低延迟音频流（Mono, f32, 48kHz）

**关键方法：**
- `start()` → 启动录音
- `into_wav_bytes()` → 停止录音并转为 WAV 字节
- `samples_since(offset)` → 获取新增样本（用于 VAD 实时处理）

### 6.5 conversation.rs — 对话处理

**文件：** `ale-gui/src/conversation.rs`

核心对话处理流程：

```
handle_question_response()
├── ask_question()
│   ├── 有图像 → engine.ask_about_image_with_tools(image, question, automation_tools())
│   └── 无图像 → engine.ask_text(question)
├── 更新 UI（转写、AI 回答）
├── record_interaction() → 更新上下文 + 自动学习记忆
├── apply_tool_calls() → 解析 Function Call → 显示操作计划
├── auto_speak → speak_and_play() → TTS 播放
└── 重启监听
```

#### automation_tools()

定义了 `execute_action_plan` 函数的 JSON Schema，包含 9 种操作类型的参数定义，通过 OpenAI Function Calling 传递给 AI。

### 6.6 screen_capture.rs — 屏幕捕获

**文件：** `ale-gui/src/screen_capture.rs`（Desktop only）

```rust
pub struct ScreenCapture {
    latest_frame: Arc<Mutex<Option<ScreenFrame>>>,
    running: Arc<Mutex<bool>>,
    config: CaptureConfig,
}
```

**CaptureConfig：**
- `interval`：截图间隔，默认 3 秒
- `scale`：缩放比例，默认 0.5
- `jpeg_quality`：JPEG 质量，默认 80

**工作流程：**
1. 后台线程每 3 秒截取主显示器
2. 缩放到 50%
3. 存储为 RGBA 帧
4. 需要时转为 JPEG 发送给 API

### 6.7 automation.rs — 桌面自动化

**文件：** `ale-gui/src/automation.rs`（Desktop only）

基于 `enigo` 的键鼠自动化引擎。

#### AutomationEngine

```rust
pub struct AutomationEngine {
    enigo: Enigo,
    config: AutomationConfig,
}
```

**支持的操作：**
- 鼠标：移动、点击（左/右/中）、双击、滚动
- 键盘：按键、组合键（Ctrl/Alt/Shift/Meta + 主键）
- 应用：打开/关闭（跨平台：macOS `open -a` / Linux 直接启动 / Windows `cmd /C start`）
- URL：打开链接（通过 `open` crate，仅允许 http/https）
- 文件：创建/删除/移动/复制/重命名（限制在用户主目录内）

**安全机制：**
- 应用名验证：拒绝路径分隔符和特殊字符
- URL 验证：仅允许 `http://` 和 `https://`
- 文件路径验证：限制在用户主目录内，拒绝 `..` 路径组件

### 6.8 camera.rs — Android 相机

**文件：** `ale-gui/src/camera.rs`

```rust
pub struct AndroidCamera {
    latest_frame: Arc<Mutex<Option<CameraFrame>>>,
    running: Arc<Mutex<bool>>,
    config: CameraConfig,  // 1280x720, 30fps
}
```

**状态：** Android Camera2 JNI 集成框架已搭建，实际图像捕获回调待实现。

**工具函数：** `yuv420_to_rgba()` — YUV_420_888 到 RGBA 颜色空间转换。

### 6.9 tts_player.rs — 语音播放

**文件：** `ale-gui/src/tts_player.rs`

- **Desktop**：使用 `rodio` 播放 WAV/MP3 音频
- **Android**：通过 JNI 调用 `android.media.MediaPlayer` 播放

### 6.10 file_picker.rs — 文件选择

**文件：** `ale-gui/src/file_picker.rs`

- **Desktop**：使用 `rfd` 原生文件对话框（支持 png/jpg/jpeg/webp）
- **Android**：通过 JNI 启动系统图片选择器（`ACTION_GET_CONTENT`）

### 6.11 android.rs — Android 入口

**文件：** `ale-gui/src/android.rs`

```rust
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    slint::android::init(app);
    let window = AppWindow::new()?;
    crate::setup_app(&window);
    window.run()
}
```

**Android 权限：**
- `INTERNET`、`RECORD_AUDIO`、`MODIFY_AUDIO_SETTINGS`
- `READ_EXTERNAL_STORAGE`、`WRITE_EXTERNAL_STORAGE`、`CAMERA`

---

## 7. Feature Flags 机制

`ale-core` 通过 Cargo feature flags 控制可选功能：

```toml
[features]
default = ["cloud"]                    # 仅云端 API
tts = ["dep:tts"]                      # 系统 TTS
cloud = []                             # 云端功能标记
local-inference = ["tts", "dep:ort", "dep:ndarray", "dep:whisper-rs"]  # 本地推理
cloud-inference = ["cloud"]            # 云端推理别名
adaptive = ["local-inference", "cloud-inference"]  # 同时启用本地和云端
```

**条件编译模式：**

```rust
#[cfg(feature = "tts")]              pub mod tts;
#[cfg(feature = "local-inference")]  pub mod asr;
#[cfg(feature = "local-inference")]  pub mod vlm;
#[cfg(feature = "local-inference")]  pub mod llm;
```

---

## 8. 数据流架构

### 8.1 语音问答流程

```
用户说话
    │
    ▼
麦克风录音 (cpal/oboe)
    │
    ▼
VAD 检测 (vad.rs) ──→ SpeechEnded
    │
    ▼
录音转 WAV (audio.rs)
    │
    ├────────────────────────────────────┐
    ▼                                    ▼
屏幕截图 (xcap)                   相机帧 (Camera2/JNI)
    │                                    │
    ├────────────────────────────────────┘
    ▼
语音识别 (ASR: whisper-rs 或 OpenAI Whisper)
    │
    ▼
视觉问答 (GPT-4o Vision + Function Calling)
    │
    ├── 文本回答 → UI 显示 + TTS 朗读
    │
    └── 工具调用 → 解析 ActionPlan → UI 显示操作计划
                              │
                              ▼
                    用户确认 → 执行键鼠操作 (enigo)
```

### 8.2 上下文构建流程

```
当前问题
    │
    ├── 检索相关长期记忆 (term-matching 评分)
    ├── 获取最近视觉帧摘要 (最多 2 帧)
    ├── 获取对话摘要 (如有压缩)
    ├── 获取最近 10 条对话
    └── 当前图像描述
    │
    ▼
构建 CloudMessage 列表 → 发送给 AI
```

---

## 9. 依赖关系图

### 9.1 Crate 级依赖

```
┌─────────────────────────────────────────────────┐
│                    ale-gui                       │
│  ┌─────────────────────────────────────────┐    │
│  │ Slint 1.16 (UI)                        │    │
│  │ cpal/oboe (音频输入)                    │    │
│  │ rodio/JNI MediaPlayer (音频播放)        │    │
│  │ xcap (屏幕截图)                         │    │
│  │ enigo (键鼠自动化)                      │    │
│  │ rfd (文件对话框)                        │    │
│  └─────────────────────────────────────────┘    │
│                      │                           │
│                      ▼                           │
│              ┌───────────────┐                    │
│              │   ale-core    │                    │
│              │               │                    │
│              │  AleEngine    │                    │
│              │  ├── config   │                    │
│              │  ├── cloud    │──→ reqwest         │
│              │  ├── vad      │                    │
│              │  ├── actions  │                    │
│              │  ├── context  │──→ chrono, uuid    │
│              │  ├── memory   │──→ walkdir         │
│              │  ├── inference│                    │
│              │  ├── manager  │                    │
│              │  ├── downloader│──→ reqwest, futures│
│              │  ├── asr [L]  │──→ whisper-rs      │
│              │  ├── vlm [L]  │──→ ort             │
│              │  ├── llm [L]  │──→ llama-cpp-rs    │
│              │  └── tts [T]  │──→ tts crate       │
│              └───────────────┘                    │
└─────────────────────────────────────────────────┘
          ▲
          │
┌─────────┴───────┐
│    ale-cli      │
│  clap (CLI)     │
│  tracing-sub    │
└─────────────────┘
```

### 9.2 平台特定依赖映射

```
Desktop (Windows/Linux/macOS):
  音频输入: cpal
  音频播放: rodio
  屏幕截图: xcap
  键鼠控制: enigo
  文件对话框: rfd
  URL 打开: open

Android:
  音频输入: oboe
  音频播放: JNI MediaPlayer
  相机: JNI Camera2
  文件选择: JNI Intent
```

---

## 10. 构建与运行

### 10.1 环境要求

- **Rust**: 1.70.0+
- **Linux 依赖**:
  ```bash
  sudo apt-get install -y libspeechd-dev libasound2-dev libfontconfig-dev \
    libpipewire-0.3-dev libwayland-dev libxrandr-dev libdbus-1-dev \
    libegl-dev libgbm-dev libxcb-shape0-dev libxcb-xfixes0-dev
  ```
- **Android**: Android NDK 27+, `cargo-apk`, Java 17

### 10.2 构建命令

```bash
# 检查整个 workspace
cargo check --workspace

# 格式化检查
cargo fmt --all -- --check

# 构建桌面 GUI
cargo build --release -p ale-gui

# 构建 CLI
cargo build --release -p ale-cli

# 构建带本地推理支持
cargo build --release -p ale-core --features local-inference

# Android 构建
export ANDROID_NDK_ROOT=/path/to/android-ndk
./scripts/package-android.sh
```

### 10.3 运行命令

```bash
# 桌面 GUI
cargo run -p ale-gui

# CLI 语音识别
cargo run -p ale-cli -- transcribe --audio input.wav

# CLI 语音合成
cargo run -p ale-cli -- synthesize --text "你好" --output output.wav

# CLI 图像描述
cargo run -p ale-cli -- describe --image screenshot.png

# CLI 测试连接
cargo run -p ale-cli -- test-connection

# CLI 查看状态
cargo run -p ale-cli -- status
```

---

## 11. CI/CD 流程

### 11.1 主工作流 (build.yml)

**触发条件：**
- push 到 master/main 分支
- 推送 `v*` 标签
- Pull Request
- 手动触发（workflow_dispatch）

**Job 流程：**

```
check (ubuntu-latest)
├── cargo fmt --all -- --check
├── cargo check --workspace
│
├──→ build-windows (windows-latest)
│    └── cargo build --release -p ale-gui
│
├──→ build-linux (ubuntu-latest)
│    └── cargo build --release -p ale-cli -p ale-gui → .deb 包
│
├──→ build-android (ubuntu-latest)
│    └── ./scripts/package-android.sh → .apk (arm64 + armv7)
│
└──→ release (ubuntu-latest)
     └── softprops/action-gh-release → GitHub Release
```

### 11.2 发布产物

| 平台 | 文件名 | 说明 |
|------|--------|------|
| Windows | `ale-my-eyes-windows.exe` | 可执行文件 |
| Linux | `ale-my-eyes_0.1.0_amd64.deb` | Debian 包 |
| Android arm64 | `ale-my-eyes-arm64.apk` | APK |
| Android armv7 | `ale-my-eyes-armv7.apk` | APK |

---

## 12. 测试体系

### 12.1 测试分布

| Crate | 测试文件 | 测试内容 |
|-------|----------|----------|
| ale-core | config.rs | 默认配置、序列化往返、验证器、配置管理器加载、旧版迁移 |
| ale-core | cloud.rs | CloudConfig 默认值、Provider 序列化、Factory 创建、响应解析 |
| ale-core | vad.rs | 静默检测、语音检测、说话结束检测、RMS 能量、自适应阈值 |
| ale-core | actions.rs | 风险等级、风险升级、操作描述、序列化、ActionPlan 解析 |
| ale-core | context.rs | 基础对话、消息构建、视觉记忆、压缩、记忆去重、清空 |
| ale-core | memory.rs | 持久化/加载、去重、搜索排序、偏好提取、frontmatter 往返、JSON 迁移 |
| ale-core | lib.rs | CloudConfig 转换、引擎状态 |
| ale-core | asr.rs | 立体声转单声道、PCM16 转 f32、重采样 |
| ale-core | llm.rs | 响应解析 |
| ale-gui | automation.rs | 按键解析、配置默认值、应用名安全验证、URL 安全验证、路径安全验证 |
| ale-gui | screen_capture.rs | 截图配置默认值 |
| ale-gui | camera.rs | YUV→RGBA 转换 |

### 12.2 测试隔离

- 所有测试使用 `/tmp/ale-my-eyes-test-*` 路径
- 使用 `uuid::Uuid::new_v4()` 确保路径唯一
- 测试结束后通过 `std::fs::remove_file` / `remove_dir_all` 清理

### 12.3 运行测试

```bash
# 运行所有测试
cargo test

# 仅运行核心库测试
cargo test -p ale-core

# 运行特定测试
cargo test -p ale-core test_default_config

# 运行 GUI 测试
cargo test -p ale-gui
```

---

## 13. 配置与数据目录

### 13.1 配置文件

| 路径 | 说明 |
|------|------|
| `~/.config/ale-my-eyes/config.json` | 用户配置文件 |
| `/tmp/ale-my-eyes-test/config.json` | 测试配置文件 |

### 13.2 数据目录

| 路径 | 说明 |
|------|------|
| `~/.local/share/ale-my-eyes/memory/` | 长期记忆目录（.md 文件） |
| `~/.local/share/ale-my-eyes/memory/MEMORY.md` | 记忆索引文件 |
| `models/` | 模型存储目录（相对路径，相对于工作目录） |

### 13.3 构建产物（不编辑）

| 路径 | 说明 |
|------|------|
| `target/` | Cargo 构建缓存 |
| `dist/` | 打包产物 |
| `release/` | `create-release.sh` 生成的源码发布 |
| `ale-my-eyes-*` | 打包目录/压缩包 |
