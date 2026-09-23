//! zankyo — exec wrapper 兼 Runtime API proxy 兼 external extension。
//!
//! `AWS_LAMBDA_EXEC_WRAPPER` から起動され、argv に渡された本来の
//! ランタイム起動コマンドを子プロセスとして実行する。
//! 失敗時は原則 fail-open: zankyo 側の問題で関数本体を止めない。
//! このファイルは入口だけを担い、起動判定は setup、配線は orchestrate へ委譲する。

use std::ffi::OsString;
use std::process::ExitCode;

const EX_USAGE: u8 = 64;

fn init_tracing() {
    // ログは stderr へ。Lambda は wrapper の stdout/stderr を関数ログに混ぜる。
    // RUST_LOG で絞れるように env-filter を既定にする。
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

fn usage() -> ExitCode {
    eprintln!("zankyo: exec wrapper for AWS Lambda.");
    eprintln!("usage: zankyo <runtime command> [args...]");
    eprintln!("(set AWS_LAMBDA_EXEC_WRAPPER=/opt/zankyo-wrapper on the function)");
    ExitCode::from(EX_USAGE)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    init_tracing();
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    if argv.is_empty() {
        // argv なし起動は 2 通り: /opt/extensions/zankyo から platform が
        // 起動する external extension と、人手での裸実行。Lambda 環境
        // 変数の有無で区別する。
        if std::env::var_os("AWS_LAMBDA_RUNTIME_API").is_some() {
            return ExitCode::from(zankyo::orchestrate::run_agent().await);
        }
        return usage();
    }
    ExitCode::from(zankyo::orchestrate::run(&argv).await)
}
