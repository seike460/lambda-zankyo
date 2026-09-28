//! 子プロセス（本来のランタイム起動コマンド）の起動と終了コード変換。
//! fail-open の最終経路である passthrough もここに置く。

use std::ffi::OsString;
use tokio::process::{Child, Command};
use tracing::warn;

pub const EX_OSERR: u8 = 71;

/// zankyo を噛ませず子プロセスだけ起動する（fail-open 経路）。
pub async fn passthrough(argv: &[OsString]) -> u8 {
    let Some((cmd, args)) = argv.split_first() else {
        warn!("passthrough called with empty argv");
        return EX_OSERR;
    };
    match Command::new(cmd).args(args).status().await {
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
    let Some((cmd, args)) = argv.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty argv",
        ));
    };
    Command::new(cmd)
        .args(args)
        .env("AWS_LAMBDA_RUNTIME_API", format!("127.0.0.1:{port}"))
        .spawn()
}

/// 子の終了ステータスをプロセスの終了コードに写す。
/// シグナル終了はシェル慣例の 128+signal に写し、ランタイムの
/// 死因（SIGSEGV 等）が wrapper の exit code から分かるようにする。
/// wait 自体の失敗や u8 に収まらない値は 1 に丸める。
pub fn exit_code(status: std::io::Result<std::process::ExitStatus>) -> u8 {
    match status {
        Ok(s) => {
            if let Some(c) = s.code() {
                return u8::try_from(c).unwrap_or(1);
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = s.signal() {
                    return 128u8.saturating_add(sig as u8);
                }
            }
            1
        }
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

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[tokio::test]
    async fn passthrough_returns_child_exit_code() {
        assert_eq!(passthrough(&argv(&["sh", "-c", "exit 7"])).await, 7);
    }

    #[tokio::test]
    async fn passthrough_maps_unspawnable_command_to_oserr() {
        assert_eq!(
            passthrough(&argv(&["/nonexistent/zankyo-runtime"])).await,
            EX_OSERR
        );
        assert_eq!(passthrough(&[]).await, EX_OSERR);
    }

    #[tokio::test]
    async fn spawn_via_proxy_points_child_at_proxy_port() {
        let mut child = spawn_via_proxy(
            &argv(&[
                "sh",
                "-c",
                r#"test "$AWS_LAMBDA_RUNTIME_API" = 127.0.0.1:4321"#,
            ]),
            4321,
        )
        .unwrap();
        assert_eq!(exit_code(child.wait().await), 0);
    }
}
