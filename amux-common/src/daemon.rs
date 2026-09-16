//! Daemon 协议：Server 与 Daemon 之间经 WebSocket 传输的 JSON-RPC 2.0 消息。
//!
//! 方法名与通知名的单一来源；参数与结果类型放在此处，供 daemon 与 server 共用。

use serde::{Deserialize, Serialize};

/// Server → Daemon 的请求方法名。
pub mod method {
    /// 获取当前机器信息（操作系统等）
    pub const MACHINE_INFO: &str = "machine.info";
    /// 发现本机已安装的 agents 及各自是否已启动
    pub const AGENT_LIST: &str = "agent.list";
    /// 启动（未启动）或重启（已启动）指定 agent
    pub const AGENT_RESTART: &str = "agent.restart";
    pub const GIT_DIFF: &str = "git.diff";
    pub const GIT_RESTORE: &str = "git.restore";
    pub const GIT_WORKTREE_NEW: &str = "git.worktree.new";
    pub const GIT_WORKTREE_RESUME: &str = "git.worktree.resume";
    pub const GIT_WORKTREE_LIST: &str = "git.worktree.list";
    pub const GIT_WORKTREE_REMOVE: &str = "git.worktree.remove";
    pub const FS_LIST: &str = "fs.list";
    pub const FS_READ: &str = "fs.read";
    pub const TERMINAL_OPEN: &str = "terminal.open";
    pub const TERMINAL_RESIZE: &str = "terminal.resize";
    pub const TERMINAL_INPUT: &str = "terminal.input";
    pub const TERMINAL_CLOSE: &str = "terminal.close";
}

/// 通知名（`acp` 双向复用，`terminal.*` 仅 Daemon → Server 上行）。
pub mod notify {
    /// ACP 消息转发：Server → Daemon 下行、Daemon → Server 上行
    pub const ACP: &str = "acp";
    /// 终端输出（Daemon → Server）
    pub const TERMINAL_OUTPUT: &str = "terminal.output";
    /// 终端进程退出（Daemon → Server）
    pub const TERMINAL_EXIT: &str = "terminal.exit";
}

/// 认证握手用的请求头名（HTTP 头名大小写不敏感，统一小写以便 `HeaderName::from_static`）。
pub mod header {
    use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

    pub const AUTHORIZATION: &str = "authorization";
    pub const MACHINE: &str = "amux-machine";

    /// 头值转义集：保留 URL 非保留字符，其余按 UTF-8 字节转义
    /// （HTTP 头值只允许可见 ASCII，机器名可以含非 ASCII）。
    const MACHINE_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');

    /// 机器名编码为 `amux-machine` 头值（docs/DESIGN.md「认证」）。
    pub fn encode_machine(name: &str) -> String {
        utf8_percent_encode(name, MACHINE_ENCODE_SET).to_string()
    }

    /// 解析 `amux-machine` 头值；转义结果不是合法 UTF-8 时返回 `None`。
    pub fn decode_machine(value: &str) -> Option<String> {
        percent_decode_str(value)
            .decode_utf8()
            .ok()
            .map(|name| name.into_owned())
    }
}

/// `machine.info` 结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MachineInfo {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub hostname: String,
    /// daemon 版本
    pub version: String,
}

/// 本机一个 agent 的发现结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredAgent {
    pub name: String,
    /// 进程是否已启动
    pub running: bool,
}

/// `agent.list` 结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentListResult {
    pub agents: Vec<DiscoveredAgent>,
}

/// `agent.restart` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentParams {
    pub agent: String,
}

/// `acp` 通知负载。`raw` 为一条 ACP JSON-RPC 消息的 JSON 文本。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AcpForward {
    pub agent: String,
    pub raw: String,
}

/// 以仓库根目录为目标的 git 参数（`git.diff` / `git.worktree.new` / `git.worktree.list`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitRepoParams {
    pub repo: String,
}

/// `git.restore` 参数：仅给文件路径时整文件撤销，给 patch 时按块撤销。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitRestoreParams {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
}

/// `git.worktree.new` / `git.worktree.resume` 结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeResult {
    pub worktree_dir: String,
}

/// `git.worktree.resume` / `git.worktree.remove` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreePathParams {
    pub repo: String,
    pub path: String,
}

/// `git.worktree.list` 结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeListResult {
    pub worktrees: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_forward_carries_agent_and_raw_message() {
        let forward = AcpForward {
            agent: "codex".into(),
            raw: r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#.into(),
        };
        let json = serde_json::to_value(&forward).unwrap();
        assert_eq!(json["agent"], "codex");
        assert!(json["raw"].as_str().unwrap().contains("initialize"));
    }

    #[test]
    fn discovered_agent_reports_running_flag() {
        let result = AgentListResult {
            agents: vec![DiscoveredAgent {
                name: "codex".into(),
                running: true,
            }],
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["agents"][0]["name"], "codex");
        assert_eq!(json["agents"][0]["running"], true);
    }

    #[test]
    fn machine_header_round_trips_non_ascii_names() {
        let name = "开发机 A";
        let encoded = header::encode_machine(name);
        assert!(
            encoded.is_ascii() && !encoded.contains(' '),
            "头值必须是可见 ASCII: {encoded}"
        );
        assert_eq!(
            encoded,
            "%E5%BC%80%E5%8F%91%E6%9C%BA%20A",
            "非保留字符保持原样，其余按 UTF-8 转义"
        );
        assert_eq!(header::decode_machine(&encoded).as_deref(), Some(name));
        assert_eq!(header::decode_machine("pc-1.local").as_deref(), Some("pc-1.local"));
        assert_eq!(header::decode_machine("%FF"), None, "非法 UTF-8 应被拒绝");
    }
}
