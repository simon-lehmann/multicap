use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[cfg(windows)]
    #[error("windows api: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Msg(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn msg(s: impl Into<String>) -> Self {
        Error::Msg(s.into())
    }
}
