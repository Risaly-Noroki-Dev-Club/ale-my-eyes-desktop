use thiserror::Error;

#[derive(Error, Debug)]
pub enum AleError {
    #[error(transparent)]
    ModelCall(#[from] crate::model_api::ModelCallError),

    #[error("Config error: {0}")]
    ConfigError(String),

    #[error("Model not initialized: {0}")]
    NotInitialized(&'static str),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Other error: {0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, AleError>;
