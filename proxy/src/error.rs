//! zankyo 全体で共有するエラー型。
//! 失敗を種類で分けて持ち、呼び出し側が「設定ミス」と「一時的な IO 失敗」を
//! 区別できるようにする（wrapper は原則 fail-open で子プロセスを起動するが、
//! 原因特定のためエラー内容はログに残す）。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ZankyoError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("upstream runtime API error: {0}")]
    Upstream(String),
    #[error("http build error: {0}")]
    Http(#[from] http::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("aws sdk error: {0}")]
    Aws(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hyper error: {0}")]
    Hyper(#[from] hyper::Error),
}

pub type Result<T> = std::result::Result<T, ZankyoError>;
