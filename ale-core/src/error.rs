use thiserror::Error;

#[derive(Error, Debug)]
pub enum AleError {
    #[error(transparent)]
    ModelCall(#[from] crate::model_api::ModelCallError),
    #[error("ASR error: {0}")]
    AsrError(String),

    #[error("TTS error: {0}")]
    TtsError(String),

    #[error("VLM error: {0}")]
    VlmError(String),

    #[error("Cloud API error: {0}")]
    CloudApiError(String),

    #[error("Config error: {0}")]
    ConfigError(String),

    #[error("Model not initialized: {0}")]
    NotInitialized(&'static str),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Image error: {0}")]
    ImageError(#[from] image::ImageError),

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Other error: {0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, AleError>;
