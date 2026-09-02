//! 测试/演示用 server 入口：
//! 启动与 `amux-server` 相同的常驻进程，但把 ACP agent 固定为仓库自带的
//! mock_acp（`tests/support/mock_acp.rs` 编译出的可执行），无需安装真实 agent。
//! 用法与 amux-server 相同：`cargo run -p amux-server --bin test-server -- --token <值> [--port N]`。
//!
//! `--data-dir` / `--agent` 仅为此演示入口保留（文档未定义 amux-server 的这两个 CLI 参数），
//! 启动 amux-server 前转换为 `AMUX_DATA_DIR` / `AMUX_AGENT_BIN` 环境变量。

use std::path::PathBuf;

/// 定位同包兄弟 bin 的可执行（与本二进制同目录：target/debug 或 target/release）。
fn sibling_bin(name: &str) -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    #[cfg(windows)]
    let name = format!("{name}.exe");
    let p = dir.join(name);
    p.exists().then_some(p)
}

/// 把 `--flag value` / `--flag=value` 转换为 (env 变量名, 值)。
fn take_value(raw: &[String], i: &mut usize, flag: &str) -> Option<String> {
    let cur = raw.get(*i)?;
    if let Some(v) = cur.strip_prefix(&format!("{flag}=")) {
        return Some(v.to_string());
    }
    if cur == flag {
        *i += 1;
        return raw.get(*i).cloned();
    }
    None
}

fn main() {
    let Some(server) = sibling_bin("amux-server") else {
        eprintln!("未找到 amux-server 可执行，请先构建：cargo build -p amux-server --bins");
        std::process::exit(1);
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut forwarded: Vec<String> = Vec::new();
    let mut data_dir = std::env::var("AMUX_DATA_DIR").ok();
    let mut agent = std::env::var("AMUX_AGENT_BIN").ok();

    let mut i = 0;
    while i < raw.len() {
        if let Some(v) = take_value(&raw, &mut i, "--data-dir") {
            data_dir = Some(v);
        } else if let Some(v) = take_value(&raw, &mut i, "--agent") {
            agent = Some(v);
        } else {
            forwarded.push(raw[i].clone());
        }
        i += 1;
    }
    if agent.is_none() {
        let mock = sibling_bin("mock_acp").unwrap_or_else(|| {
            eprintln!("未找到 mock_acp 可执行，请先构建：cargo build -p amux-server --bins");
            std::process::exit(1);
        });
        agent = Some(mock.display().to_string());
    }

    let mut cmd = std::process::Command::new(&server);
    cmd.args(&forwarded);
    if let Some(dir) = data_dir {
        cmd.env("AMUX_DATA_DIR", dir);
    }
    cmd.env("AMUX_AGENT_BIN", agent.expect("agent 已确定"));

    // Unix 下 exec 替换当前进程：信号（Ctrl+C）与退出码自然透传，不留孤儿进程；
    // 其他平台回退为子进程方式。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let path = server.display().to_string();
        let err = cmd.exec();
        eprintln!("exec {path} 失败: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().expect("启动 server 失败");
        std::process::exit(status.code().unwrap_or(1));
    }
}
