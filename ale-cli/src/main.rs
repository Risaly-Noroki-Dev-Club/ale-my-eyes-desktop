use ale_core::AleEngineFactory;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "ale-cli")]
#[command(about = "CLI tool for Ale, My Eyes!")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 语音识别
    Transcribe {
        /// 音频文件路径
        #[arg(short, long)]
        audio: PathBuf,

        /// 输出文本文件路径
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// 图像描述
    Describe {
        /// 图像文件路径
        #[arg(short, long)]
        image: PathBuf,

        /// 输出文本文件路径
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// 测试云端连接
    TestConnection,

    /// 显示状态
    Status,

    /// 在模拟桌面运行任务闭环，输出 JSON 证据；不操作真实桌面或调用云端
    HarnessDemo {
        #[arg(long, default_value = "zoom")]
        task: String,
        #[arg(long, default_value = "normal")]
        scenario: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::HarnessDemo {
            task,
            scenario,
            output,
        } => {
            let scenario = scenario.parse().map_err(anyhow::Error::msg)?;
            let report = ale_core::harness::simulation::demo(&task, scenario)
                .await
                .map_err(anyhow::Error::msg)?;
            write_or_print(output, &serde_json::to_string_pretty(&report)?).await?;
            if report.status != ale_core::harness::Status::Completed {
                std::process::exit(2);
            }
        }
        Commands::Transcribe { audio, output } => {
            let engine = AleEngineFactory::create_default().await?;
            print_config_warnings(&engine);
            let audio_data = tokio::fs::read(&audio).await?;
            let text = engine.transcribe(&audio_data).await?;
            write_or_print(output, &text).await?;
        }
        Commands::Describe { image, output } => {
            let engine = AleEngineFactory::create_default().await?;
            print_config_warnings(&engine);
            let image_data = tokio::fs::read(&image).await?;
            let description = engine.describe_image(&image_data).await?;
            write_or_print(output, &description).await?;
        }
        Commands::TestConnection => {
            let engine = AleEngineFactory::create_default().await?;
            print_config_warnings(&engine);
            match engine.test_cloud_api().await {
                Ok(true) => {
                    println!("云端连接测试成功");
                    let config = engine.config();
                    println!("API URL: {}", config.cloud_api.api_url);
                    println!("Model: {}", config.cloud_api.model);
                }
                Ok(false) => {
                    println!("云端连接测试失败");
                    std::process::exit(1);
                }
                Err(error) => {
                    println!("云端连接测试失败: {error}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Status => {
            let engine = AleEngineFactory::create_default().await?;
            print_config_warnings(&engine);
            let status = engine.status().await;
            let config = engine.config();
            println!("Ale, My Eyes! CLI");
            println!("Version: {}", env!("CARGO_PKG_VERSION"));
            println!("Cloud ready: {}", status.cloud_ready);
            println!("API URL: {}", config.cloud_api.api_url);
            println!("Model: {}", config.cloud_api.model);
            println!("Language: {}", config.ui.language);
        }
    }

    Ok(())
}

/// headless/无 D-Bus 环境下凭据读取降级为警告；只读命令仍可运行（DC-03）。
fn print_config_warnings(engine: &ale_core::AleEngine) {
    for warning in engine.credential_warnings() {
        eprintln!("warning: {warning}");
    }
}

async fn write_or_print(output: Option<PathBuf>, text: &str) -> anyhow::Result<()> {
    if let Some(output) = output {
        tokio::fs::write(&output, text).await?;
        println!("Text written to {}", output.display());
    } else {
        println!("{text}");
    }

    Ok(())
}
