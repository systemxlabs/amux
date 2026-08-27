//! ACP agent 自动发现：PATH 探测已知 CLI，kimi 使用原生 `acp` 子命令，
//! grok 使用原生 `agent` 子命令，claude/codex 使用 npx 官方包装器。
//! 发现结果由 `AgentRegistry::launch_discovered` 在 server 启动时并行拉起。

/// 自动发现的 ACP agent（含 ACP 子命令参数 / npx 包装器参数与附加环境变量）。
#[derive(Debug, Clone)]
pub struct DiscoveredAgent {
    pub name: String,
    pub bin: String,
    pub args: Vec<String>,
    /// 附加环境变量（如 codex 包装器的 INITIAL_AGENT_MODE）
    pub env: Vec<(String, String)>,
}

/// 自动发现已知的 ACP agent：
/// - kimi：装有 `kimi` CLI 且 `kimi acp --help` 可用 → `kimi acp`
/// - grok：装有 `grok` CLI 且 `grok agent --help` 可用 → `grok agent --always-approve stdio`
/// - claude：装有 `claude` CLI 且 npx 可用 → `npx -y @agentclientprotocol/claude-agent-acp`
/// - codex：装有 `codex` CLI 且 npx 可用 → `npx -y @agentclientprotocol/codex-acp`
///
/// 不做任意 `*-acp` 扫描：只认已知 agent，避免无关可执行污染列表。
/// 发现的 agent 由 `launch_discovered` 在 server 启动时拉起。
pub(crate) fn discover_acp_agents() -> Vec<DiscoveredAgent> {
    let mut found: Vec<DiscoveredAgent> = Vec::new();
    let npx = find_on_path("npx");
    for (cli, pkg) in [
        ("kimi", None),
        ("claude", Some("@agentclientprotocol/claude-agent-acp")),
        ("codex", Some("@agentclientprotocol/codex-acp")),
    ] {
        let cli_bin = find_on_path(cli);
        let acp_supported = cli_bin.as_deref().map(has_acp_subcommand).unwrap_or(false);
        if let Some(d) = discover_for_cli(cli, pkg, cli_bin, acp_supported, npx.clone()) {
            found.push(d);
        }
    }
    let grok_bin = find_on_path("grok");
    let agent_supported = grok_bin.as_deref().map(has_agent_subcommand).unwrap_or(false);
    if let Some(d) = discover_for_grok(grok_bin, agent_supported) {
        found.push(d);
    }
    found
}

/// 单 CLI 的发现决策（纯逻辑，便于单测）。
fn discover_for_cli(
    cli: &str,
    pkg: Option<&str>,
    cli_bin: Option<String>,
    acp_supported: bool,
    npx_bin: Option<String>,
) -> Option<DiscoveredAgent> {
    let cli_bin = cli_bin?;
    match (cli, pkg) {
        // 只有确认支持 `acp` 子命令时才使用原生模式。
        ("kimi", _) if acp_supported => Some(DiscoveredAgent {
            name: cli.to_string(),
            bin: cli_bin,
            args: vec!["acp".to_string()],
            env: Vec::new(),
        }),
        // claude/codex 始终走官方包装器，避免依赖 CLI 自带的 ACP 实现。
        ("claude" | "codex", Some(pkg)) => {
            let npx = npx_bin?;
            let mut env = Vec::new();
            if cli == "codex" {
                env.push(("INITIAL_AGENT_MODE".into(), "agent-full-access".into()));
            }
            Some(DiscoveredAgent {
                name: cli.to_string(),
                bin: npx,
                args: vec!["-y".to_string(), pkg.to_string()],
                env,
            })
        }
        _ => None,
    }
}

/// grok 的发现决策（纯逻辑，便于单测）。
fn discover_for_grok(cli_bin: Option<String>, agent_supported: bool) -> Option<DiscoveredAgent> {
    let cli_bin = cli_bin?;
    if !agent_supported {
        return None;
    }
    Some(DiscoveredAgent {
        name: "grok".to_string(),
        bin: cli_bin,
        args: vec![
            "agent".to_string(),
            "--always-approve".to_string(),
            "stdio".to_string(),
        ],
        env: Vec::new(),
    })
}

/// 在 PATH 上查找可执行文件（含 `.exe` 后缀剥离）。
fn find_on_path(name: &str) -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path) {
        for candidate in [name, &format!("{name}.exe")] {
            let p = dir.join(candidate);
            if p.is_file() && is_executable(&p) {
                return Some(p.display().to_string());
            }
        }
    }
    None
}

/// 探测 `<bin> acp --help`：退出码 0 且输出提及 acp（区分"有 acp 子命令"与
/// "未知子命令回落通用帮助"——codex 0.137 退出 0 但输出不含 acp，故被排除）。
fn has_acp_subcommand(bin: &str) -> bool {
    use std::process::Command;
    let Ok(out) = Command::new(bin).args(["acp", "--help"]).output() else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
    text.contains("acp")
}

/// 探测 `<bin> agent --help`：退出码 0 且输出提及 agent，
/// 用于确认 grok CLI 的 `agent` 子命令可用。
fn has_agent_subcommand(bin: &str) -> bool {
    use std::process::Command;
    let Ok(out) = Command::new(bin).args(["agent", "--help"]).output() else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
    text.contains("agent")
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &std::path::Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_acp_subcommand_detects() {
        assert!(!has_acp_subcommand("/nonexistent/bin/definitely-not-here"));
    }

    #[test]
    fn discover_for_cli_prefers_native_acp() {
        let d = discover_for_cli(
            "kimi",
            None,
            Some("/usr/bin/kimi".into()),
            true,
            Some("/usr/bin/npx".into()),
        )
        .unwrap();
        assert_eq!(d.name, "kimi");
        assert_eq!(d.bin, "/usr/bin/kimi");
        assert_eq!(d.args, vec!["acp"]);
        assert!(d.env.is_empty());
    }

    #[test]
    fn discover_for_cli_claude_via_npx() {
        for acp_supported in [false, true] {
            let d = discover_for_cli(
                "claude",
                Some("@agentclientprotocol/claude-agent-acp"),
                Some("/usr/bin/claude".into()),
                acp_supported,
                Some("/usr/bin/npx".into()),
            )
            .unwrap();
            assert_eq!(d.name, "claude");
            assert_eq!(d.bin, "/usr/bin/npx");
            assert_eq!(d.args, vec!["-y", "@agentclientprotocol/claude-agent-acp"]);
            assert!(d.env.is_empty());
        }
        // kimi 无 acp 子命令时不发现（文档无 npx 回落路径）
        assert!(
            discover_for_cli("kimi", None, Some("/usr/bin/kimi".into()), false, None).is_none()
        );
    }

    #[test]
    fn discover_for_cli_codex_via_npx_with_env() {
        let d = discover_for_cli(
            "codex",
            Some("@agentclientprotocol/codex-acp"),
            Some("/usr/bin/codex".into()),
            false,
            Some("/usr/bin/npx".into()),
        )
        .unwrap();
        assert_eq!(d.name, "codex");
        assert_eq!(d.bin, "/usr/bin/npx");
        assert_eq!(d.args, vec!["-y", "@agentclientprotocol/codex-acp"]);
        assert_eq!(
            d.env,
            vec![(
                "INITIAL_AGENT_MODE".to_string(),
                "agent-full-access".to_string()
            )]
        );
    }

    #[test]
    fn discover_for_cli_missing_prereqs() {
        assert!(discover_for_cli(
            "codex",
            Some("@agentclientprotocol/codex-acp"),
            None,
            false,
            Some("/usr/bin/npx".into()),
        )
        .is_none());
        assert!(discover_for_cli(
            "codex",
            Some("@agentclientprotocol/codex-acp"),
            Some("/usr/bin/codex".into()),
            false,
            None,
        )
        .is_none());
    }

    #[test]
    fn discover_for_grok_native() {
        let d = discover_for_grok(Some("/usr/bin/grok".into()), true).unwrap();
        assert_eq!(d.name, "grok");
        assert_eq!(d.bin, "/usr/bin/grok");
        assert_eq!(d.args, vec!["agent", "--always-approve", "stdio"]);
        assert!(d.env.is_empty());
    }

    #[test]
    fn discover_for_grok_missing_or_unsupported() {
        assert!(discover_for_grok(None, true).is_none());
        assert!(discover_for_grok(Some("/usr/bin/grok".into()), false).is_none());
    }

    #[test]
    fn has_agent_subcommand_detects() {
        assert!(!has_agent_subcommand("/nonexistent/bin/definitely-not-here"));
    }
}
