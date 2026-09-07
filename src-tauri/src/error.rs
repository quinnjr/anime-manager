use serde::Serialize;

#[derive(Debug, thiserror::Error, Serialize, Clone, PartialEq)]
#[serde(tag = "kind", content = "message")]
pub enum AppError {
    #[error("io: {0}")]
    Io(String),
    #[error("db: {0}")]
    Db(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("network: {0}")]
    Network(String),
    #[error("player: {0}")]
    Player(String),
}

pub type Result<T> = std::result::Result<T, AppError>;

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self { AppError::Io(e.to_string()) }
}
impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self { AppError::Db(e.to_string()) }
}
impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self { AppError::Network(e.to_string()) }
}
impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self { AppError::Parse(e.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_kind_and_message() {
        let json = serde_json::to_string(&AppError::Player("mpv not found".into())).unwrap();
        assert_eq!(json, r#"{"kind":"Player","message":"mpv not found"}"#);
    }
}
