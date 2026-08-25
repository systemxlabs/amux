//! 测试/演示用 server 入口（docs/DESIGN.md §9）：
//! 启动与 `amux-server` 相同的常驻进程，但把 ACP agent 固定为仓库自带的
//! mock_acp（`tests/support/mock_acp.rs` 编译出的可执行），无需安装真实 agent。
//! 用法与 amux-server 相同：`cargo run -p amux-server --bin test-server -- --token <值> [--port N]`。
//! 显式传入 `--agent` 时不覆盖（按用户指定）。

use std::path::PathBuf;

/// 定位同包兄弟 bin 的可执行（与本二进制同目录：target/debug 或 target/release）。
fn sibling_bin(name: &str) -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    #[cfg(windows)]
    let name = format!("{name}.exe");
    let p = dir.join(name);
    p.exists().then_some(p)
}

fn main() {
    let Some(server) = sibling_bin("amux-server") else {
        eprintln!("未找到 amux-server 可执行，请先构建：cargo build -p amux-server --bins");
        std::process::exit(1);
    };
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--agent") {
        let mock = sibling_bin("mock_acp").unwrap_or_else(|| {
            eprintln!("未找到 mock_acp 可执行，请先构建：cargo build -p amux-server --bins");
            std::process::exit(1);
        });
        args.push("--agent".into());
        args.push(mock.display().to_string());
    }
    // Unix 下 exec 替换当前进程：信号（Ctrl+C）与退出码自然透传，不留孤儿进程；
    // 其他平台回退为子进程方式。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let path = server.display().to_string();
        let err = std::process::Command::new(server).args(&args).exec();
        eprintln!("exec {path} 失败: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(server)
            .args(&args)
            .status()
            .expect("启动 server 失败");
        std::process::exit(status.code().unwrap_or(1));
    }
}
