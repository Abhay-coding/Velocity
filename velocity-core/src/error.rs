use thiserror::Error;

#[derive(Error, Debug)]
pub enum VelocityError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Index provider error: {0}")]
    IndexProvider(String),

    #[error("Everything not found: {0}")]
    EverythingNotFound(String),

    #[error("Invalid query: {0}")]
    InvalidQuery(String),

    #[error("Not implemented: {0}")]
    NotImplemented(String),

    #[error("Platform error: {0}")]
    Platform(#[from] slint::PlatformError),
}

pub type VelocityResult<T> = Result<T, VelocityError>;