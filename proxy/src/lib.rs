//! zankyo ライブラリ境界。
//! 実行入口 (main.rs) は薄く保ち、全機能をこのクレートの公開モジュールとして
//! 構成する。テスト (tests/) はこの公開 API だけを使い、内部実装に触れない。
#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod extension;
pub mod inflight;
pub mod proxy;
pub mod record;
pub mod runtime;
pub mod scrub;
mod scrub_data;
pub mod setup;
pub mod ssm;
pub mod store;
pub mod upstream;
