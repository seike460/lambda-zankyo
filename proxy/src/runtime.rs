//! 子プロセス（本来のランタイム起動コマンド）の起動と終了コード変換。
//! fail-open の最終経路である passthrough もここに置く。

use std::ffi::OsString;
use tokio::process::{Child, Command};
use tracing::warn;

pub const EX_OSERR: u8 = 71;

/// zankyo を噛ませず子プロセスだけ起動する（fail-open 経路）。
pub async fn passthrough(argv: &[OsString]) -> u8 {
    match Command::new(&argv[0]).args(&argv[1..]).status().await {
        Ok(s) => exit_code(Ok(s)),
        Err(e) => {
            warn!(error = %e, "failed to spawn child process");
            EX_OSERR
        }
    }
}

/// proxy 経由で子を起動する。`AWS_LAMBDA_RUNTIME_API` をこちらの
/// listen ポートに張り替えて渡す。
pub fn spawn_via_proxy(argv: &[OsString], port: u16) -> std::io::Result<Child> {
    Command::new(&argv[0])
        .args(&argv[1..])
        .env("AWS_LAMBDA_RUNTIME_API", format!("127.0.0.1:{port}"))
        .spawn()
}

/// 子の終了ステータスをプロセスの終了コードに写す。
/// code を持たない（シグナル終了等）・wait 自体の失敗・u8 に
/// 収まらない値は 1 に丸める。
pub fn exit_code(status: std::io::Result<std::process::ExitStatus>) -> u8 {
    match status {
        Ok(s) => s.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(1),
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_passes_through_child_status() {
        let status = std::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .status();
        assert_eq!(exit_code(status), 3);
    }

    #[test]
    fn exit_code_maps_spawn_failure_to_one() {
        let err = std::io::Result::Err(std::io::Error::other("x"));
        assert_eq!(exit_code(err), 1);
    }
}
