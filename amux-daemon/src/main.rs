//! amux Daemon：机器常驻进程。
//!
//! 职责（docs/DESIGN.md「Daemon」一节）：Agent 多路复用（stdio 转发）、Agent 生命周期
//! 执行、终端、git 与 fs 等与机器绑定的能力；作为 Server 与 Agent 之间的桥梁。

mod agents;
mod connection;
mod daemon_lock;
mod frames;
mod fs;
mod git;
mod machine;
mod nano;
mod outbox;
mod rpc;
mod terminal;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "amux-daemon", about = "机器常驻进程：经 WebSocket 对接 Server")]
struct Args {
    /// 机器名称（不得包含路径分隔符）
    #[arg(long, value_parser = parse_machine_name)]
    machine: String,
    /// Server 的 WebSocket 地址（如 ws://127.0.0.1:34567）
    #[arg(long)]
    server: String,
    /// 认证 token
    #[arg(long)]
    token: String,
}

fn parse_machine_name(value: &str) -> Result<String, String> {
    if value.contains(['/', '\\']) {
        return Err("机器名称不得包含路径分隔符".to_string());
    }
    Ok(value.to_string())
}

fn main() {
    amux_common::log::init_file_output(&amux_common::log::log_path("daemon"));
    let args = Args::parse();
    let _daemon_lock = match daemon_lock::acquire(&args.machine) {
        Ok(lock) => {
            log::info!(
                "已获取 Daemon 排他锁: {}",
                daemon_lock::lock_path(&args.machine).display()
            );
            lock
        }
        Err(error) => {
            log::error!("daemon 启动失败: {error}");
            std::process::exit(1);
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio runtime 失败");
    if let Err(error) = runtime.block_on(connection::run(args.machine, args.server, args.token)) {
        log::error!("daemon 退出: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_name_rejects_path_separators() {
        assert_eq!(parse_machine_name("开发机 A").unwrap(), "开发机 A");
        assert!(parse_machine_name("machine/name").is_err());
        assert!(parse_machine_name("machine\\name").is_err());
    }
}
