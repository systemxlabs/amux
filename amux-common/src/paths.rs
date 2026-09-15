//! amux 运行目录（默认 `~/.amux`，可用 `AMUX_HOME` 覆盖，便于测试与多实例）。

use std::path::PathBuf;

/// amux 运行目录：`AMUX_HOME` 优先，否则 `$HOME/.amux`。
pub fn amux_home() -> PathBuf {
    if let Ok(home) = std::env::var("AMUX_HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".amux")
}

/// Git worktree 根目录：`<amux_home>/worktrees`。
pub fn worktrees_dir() -> PathBuf {
    amux_home().join("worktrees")
}

/// 日志目录：`<amux_home>/logs`。
pub fn logs_dir() -> PathBuf {
    amux_home().join("logs")
}

/// 配置文件路径：`<amux_home>/config/<name>.json`。
pub fn config_file(name: &str) -> PathBuf {
    amux_home().join("config").join(format!("{name}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_live_under_dot_amux() {
        // 不依赖 AMUX_HOME：仅验证拼接规则
        let home = amux_home();
        assert!(home.ends_with(".amux") || std::env::var("AMUX_HOME").is_ok());
        assert!(worktrees_dir().ends_with("worktrees"));
        assert!(logs_dir().ends_with("logs"));
        assert!(config_file("skills").ends_with("config/skills.json"));
    }
}
