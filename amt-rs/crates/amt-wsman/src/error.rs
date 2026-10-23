use thiserror::Error;

#[derive(Debug, Error)]
pub enum AmtWsManError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Serialization error: {0}")]
    Serde(String),
    // Add more as needed
}

pub type AmtWsManResult<T> = Result<T, AmtWsManError>;
