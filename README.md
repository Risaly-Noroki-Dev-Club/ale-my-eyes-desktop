# Ale, My Eyes! Desktop

> **Beta** — AME的第一个公开版本就此开始。

## 它是什么

Ale, My Eyes! 桌面端是你电脑上一个比较好的无障碍工具。

对于视障用户，它是通向数字世界的另一双手：你说话，它倾听；你提问，它观察屏幕并回答；你请求帮助，它先确认你的意图，再谨慎地执行。对于开发者和技术爱好者，它展示了一种可能：人工智能不必运行在遥远的云端，而是可以在你的桌面上，安静地、私密地、可靠地为你服务。

这个桌面端完全独立于移动端代码库。它拥有自己的核心（`ale-core`）、自己的图形界面（`ale-gui`）、自己的命令行工具（`ale-cli`）和一个新的模型调用工具（`ale-modeld`）。

## 核心能力

### 持续语音交互

启动移动端，坐在电脑前，开始说话。应用会持续监听（但本阶段不会上传任何数据到互联网），你可以问："屏幕上有什么？""帮我写一封邮件。""这个按钮是做什么的？"

### 屏幕与视觉理解

应用可以捕获你的屏幕画面，交给视觉语言模型分析。它不会擅自行动——每次涉及点击、输入或修改的操作，都会先向你描述它打算做什么，等待你的确认。

### 本地模型调度器 (`ale-modeld`)

这是桌面端的新器官。它管理着本地 AI 模型的生命周期：

- **SenseVoiceSmall**：本地语音识别，将你的语音转成文字。运行在你的 CPU 上，不需要联网。
- **Qwen2.5-VL**：本地视觉语言模型，理解屏幕内容。需要 GPU 支持（Vulkan）。
- **ShowUI**：本地 UI 理解模型，定位屏幕上的可交互元素。同样需要 GPU。

`ale-modeld` 会智能地调度这些模型：同一个模型请求会复用已加载的进程；切换模型时会优雅地卸载前一个；闲置 120 秒后自动释放资源，避免一直占用你的显卡内存。如果检测到 GPU 内存不足，它会禁用该模型能力，直到你重新配置或重启。

**注意**：本地模型需要单独下载。完整模型集约需 31 GB 下载空间和 40 GB 解压空间。

Windows 用户可以使用以下脚本下载：

```bat
scripts\download-models.bat
```

### 加密远程会话（协议3）

桌面端可以作为一个安全的本地服务器，让你的 Android 手机连接进来。这一切通过 **Noise-over-WebSocket** 加密，**协议3** 带来了：

- 实时进度反馈（"正在分析屏幕...""等待你的确认..."）
- 显式的隐私/风险决策（高危操作需要人工确认）
- 显示内容和语音内容的脱敏（屏幕描述和朗读内容可以不同）
- 自动拒绝旧版本客户端，防止兼容性问题

配对是通过扫描二维码完成的，配对码仅存于内存，断开后即消失。

## 隐私

我们相信隐私是我们服务的重中之重。为了保护您的隐私，我们一直在寻求让AME最大限度保护您的隐私。

- **本地优先**：默认配置下，你的语音、屏幕截图、操作记录不会离开你的电脑。
- **云端可选**：如果你选择使用 OpenAI、Anthropic、Google 或其他云服务商，API 密钥存储在操作系统密钥库中，从不写入普通配置文件。
- **HTTPS 强制**：云 API 端点必须基于 HTTPS，明文 HTTP 会被拒绝。
- **透明**：每次自动化操作前，你会确切知道电脑打算做什么。

## 开发

```bash
# 启动图形界面
cargo run -p ale-gui

# 查看配置状态
cargo run -p ale-cli -- status

# 运行核心测试
cargo test -p ale-core

# 运行完整 CI 检查
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
./scripts/verify-pc-issues.sh
./scripts/stress-pc-io.sh
./scripts/test-source-package.sh
./scripts/smoke-linux.sh
```

Linux 开发需要安装系统依赖：

```bash
sudo apt-get install -y libclang-dev libspeechd-dev libasound2-dev libfontconfig-dev libpipewire-0.3-dev libwayland-dev libxrandr-dev libdbus-1-dev libegl-dev libgbm-dev libxcb-shape0-dev libxcb-xfixes0-dev netcat-openbsd xvfb
```

## Nix / NixOS

```bash
nix run .          # 启动 GUI
nix run .#cli -- status   # CLI
nix develop        # 进入开发 shell
```

NixOS 模块：

```nix
programs.ale-my-eyes.enable = true;
```

## 打包

```bash
./scripts/package-linux.sh    # Linux tar.gz
./scripts/package-windows.sh  # Windows zip
```


## 与移动端的关系

桌面端和移动端是两个独立的系统，通过加密协议对话：

- **移动端**（`ale-my-eyes-mobile`）：轻量的 Android 客户端，负责收音和播报。
- **桌面端**（`ale-my-eyes-desktop`）：重型的能力中心，负责理解、决策和执行。

两者各自维护自己的 `ale-core` 副本，没有跨仓库的 Cargo 依赖。当共享协议或核心行为发生变化时，需要在两边分别验证。

## 致使用者

如果你是一位视障用户，第一次使用这个软件：我们很抱歉，它还不够好。有时候会听错、看错、或者过于谨慎地反复询问。但它在学习进化，我们会尽力让AME变得更好。

如果你是一位开发者，想要贡献代码：感谢您的支持，也请始终把"安全"和"尊重"放在第一位。这里的每一行代码，最终都会影响到某个人如何与数字世界互动。

谢谢你愿意尝试 Ale, My Eyes!。

---

*Ale, My Eyes! 是开源的，属于每一个需要它的人。*

*移动端地址：https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-mobile*
