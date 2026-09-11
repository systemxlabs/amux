//! ACP agent 自动发现：PATH 探测已知 CLI，经 npx 启动官方 v2 包装器。
//! 发现结果由 `AgentRegistry::launch_discovered` 在 server 启动时并行拉起。
//!
//! 目前只有 codex 的 v2 适配器（`@nyssance/codex-acp-v2`，ACP v2 only）；
//! 其他 agent 尚无 v2 适配器，待其发布后在此追加。

/// codex 的 v2 适配器包（钉死版本：第三方包，升级需显式改这里）。
pub(crate) const CODEX_ACP_V2_PACKAGE: &str = "@nyssance/codex-acp-v2@0.6.0";

/// 自动发现的 ACP agent（含 npx 包装器参数与附加环境变量）。
#[derive(Debug, Clone)]
pub struct DiscoveredAgent {
    pub name: String,
    pub bin: String,
    pub args: Vec<String>,
    /// 附加环境变量（如 codex 包装器的 INITIAL_AGENT_MODE）
    pub env: Vec<(String, String)>,
}

/// 自动发现已知的 ACP agent：
/// - codex：装有 `codex` CLI 且 npx 可用 → `npx -y @nyssance/codex-acp-v2@<version>`
///
/// 不做任意 `*-acp` 扫描：只认已知 agent，避免无关可执行污染列表。
pub(crate) fn discover_acp_agents() -> Vec<DiscoveredAgent> {
    let mut found: Vec<DiscoveredAgent> = Vec::new();
    let codex = find_on_path("codex");
    if let Some(agent) = discover_codex(codex, find_on_path("npx")) {
        found.push(agent);
    }
    found
}

/// codex 的发现决策（纯逻辑，便于单测）：
/// 需要 `codex` CLI（适配器驱动它）与 `npx`（拉起适配器）同时具备。
fn discover_codex(codex_bin: Option<String>, npx_bin: Option<String>) -> Option<DiscoveredAgent> {
    codex_bin?;
    let npx = npx_bin?;
    Some(DiscoveredAgent {
        name: "codex".to_string(),
        bin: npx,
        args: vec!["-y".to_string(), CODEX_ACP_V2_PACKAGE.to_string()],
        env: vec![(
            "INITIAL_AGENT_MODE".to_string(),
            "agent-full-access".to_string(),
        )],
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
    fn discover_codex_via_npx_with_env() {
        let d = discover_codex(Some("/usr/bin/codex".into()), Some("/usr/bin/npx".into())).unwrap();
        assert_eq!(d.name, "codex");
        assert_eq!(d.bin, "/usr/bin/npx");
        assert_eq!(d.args, vec!["-y", CODEX_ACP_V2_PACKAGE]);
        assert_eq!(
            d.env,
            vec![(
                "INITIAL_AGENT_MODE".to_string(),
                "agent-full-access".to_string()
            )]
        );
    }

    #[test]
    fn discover_codex_requires_cli_and_npx() {
        assert!(discover_codex(None, Some("/usr/bin/npx".into())).is_none());
        assert!(discover_codex(Some("/usr/bin/codex".into()), None).is_none());
    }
}
